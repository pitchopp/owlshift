//! `owlshift do TICKET` and `owlshift resume TICKET`: one ticket turned into
//! a verified pull request, on demand and in the foreground (roadmap P1 and
//! P2, build plan OWL-20 and "The answer check").
//!
//! Both take the project's lock, refuse a project a previous run may have
//! tampered with ([`crate::project`]), read the ticket, bring the dedicated
//! checkout up to date and read the ticket's ref ([`crate::ticket_ref`]).
//! Before a Build, the forge is checked to answer and the project's rules are
//! read at the base commit ([`crate::rules`]). The Build runs through the
//! executor, following the core state machine: a failed run, a red project
//! gate included, gets one more run with the failure in its brief, and a
//! second one parks the ticket. A Build `done` whose gate passed is delivered
//! by the Writer: the gated commit pushed to the ticket's branch, the pull
//! request opened or found, its head checked to be that commit, its check
//! set read, and the delivery report posted on the ticket. Every step is an
//! event ([`crate::events`]).
//!
//! A Build that asks questions opens a round: the Writer posts them as a
//! QUESTIONS comment, and the ticket ref keeps the ask and the core state.
//! While they wait, `do` is refused; `resume` runs the answer check once the
//! decider has answered ([`crate::answer_check`]), then resumes the Build
//! after a RESUME comment of what was understood, asks again what is
//! missing, or parks the ticket. Every park, of a Build or of an answer
//! check, posts a PARKED comment saying why and what restarts it. In this
//! version the build stage is the whole pipeline, and the tracker's visible
//! stage is not moved.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::num::NonZeroU32;
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
    Author, Brief, Checkpoint, GateFailure, PermissionLevel, Permissions, Relation, Rule,
    ThreadEntry, TicketBrief,
};
use owlshift_contracts::comment::MarkedComment;
use owlshift_contracts::config::{ProjectConfig, TrackerKind};
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_contracts::refs::{Ask, AskDecider, AskKind, PersistedState, TicketQuestions};
use owlshift_contracts::result::{
    self, AnswerClass, Decision, Followup, Question, RunResult, Verdict as AnswerVerdict,
};
use owlshift_contracts::{Role, Stage, Variant};
use owlshift_core::decider::{self, Decider, NoDecider, ZoneOwners, brief_zones, declared_zones};
use owlshift_core::pipeline::Pipeline;
use owlshift_core::resource::Resource;
use owlshift_core::state::{Event, MAX_REASKS, ParkReason, Status, TicketState, Transition};

use crate::agent_env::AgentEnv;
use crate::answer_check;
use crate::events::{Data, EventSink, data};
use crate::executor::{
    DEFAULT_GATE_TIMEOUT, Executor, Git, Harness, Outcome, RESULT_PATH, RUN_DIR, RunReport, RunSpec,
};
use crate::project::{self, Base, ProjectDirs, ProjectLock};
use crate::rules;
use crate::ticket_ref::{self, Stored, TicketRecord};
use crate::writer::{
    DeliveryReport, Gate, ParkedComment, QuestionsComment, ReaskComment, Restart, ResumeComment,
    Writer, park_reason,
};

/// How long one Build run may take before its process tree is stopped.
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// How long one answer check may take: it reads a brief and writes its
/// verdicts.
pub const ANSWER_CHECK_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The forge hosts the executor's credential check asks git and gh about.
pub const FORGE_HOSTS: &[&str] = &["github.com"];

/// How many times the pull request is read before its head must be the
/// pushed commit: GitHub updates a pull request's head shortly after a push.
const HEAD_READS: u32 = 5;

/// The pipeline of this version: the build stage alone, in the core machine.
const PIPELINE: Pipeline = Pipeline::new(Variant::Trivial);

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

/// One `owlshift do` or `owlshift resume`: the adapters, the executor and
/// the project.
pub struct OnDemand<'a> {
    pub executor: &'a Executor,
    pub tracker: &'a dyn Tracker,
    pub forge: &'a GitHubForge,
    /// Runs the build role.
    pub build: &'a dyn Harness,
    /// Runs the answer check (`roles/answer_check.md`).
    pub answer_check: &'a dyn Harness,
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

/// Why `owlshift do` or `owlshift resume` stopped short of a delivery.
#[derive(Clone, Debug, PartialEq)]
pub enum Stop {
    /// Another `owlshift do` holds the project.
    Busy(PathBuf),
    /// Nothing ran, or nothing more could: the reason and its fix.
    Refused(String),
    /// A previous run's isolation check did not pass, or never ran.
    Unverified { marker: PathBuf, text: String },
    /// The run asked questions, or found the ticket's premise false. `posted`
    /// is the round the questions were posted as on the ticket, or why they
    /// were not.
    NeedsInput {
        ticket: TicketId,
        status: result::Status,
        summary: String,
        questions: Vec<Question>,
        posted: Result<NonZeroU32, String>,
    },
    /// Questions wait for the decider, who has not commented since they
    /// were asked, or since the last answer check read their answers.
    Waiting {
        ticket: TicketId,
        round: u32,
        decider: String,
        since: Timestamp,
    },
    /// The answers left questions open: they were asked again on the ticket.
    Reasked {
        ticket: TicketId,
        round: NonZeroU32,
        /// Which re-ask of the round this was, from 1.
        reask: u32,
        /// The questions asked again, each with the answer check's verdict.
        open: Vec<(Question, AnswerVerdict)>,
    },
    /// The decider asked a counter-question instead of answering; the
    /// verdicts that say so.
    CounterQuestion {
        ticket: TicketId,
        asked: Vec<AnswerVerdict>,
    },
    /// The answer check failed; it runs again on the same answers.
    CheckFailed { ticket: TicketId, detail: String },
    /// The core machine parked the ticket. `unposted` says why the PARKED
    /// comment is not on the ticket, when it is not.
    Parked {
        reason: ParkReason,
        detail: String,
        unposted: Option<String>,
    },
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
                ticket,
                status,
                summary,
                questions,
                posted,
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
                match posted {
                    Ok(round) => write!(
                        f,
                        "\nThe questions are on the ticket as round {round}: answer there, then \
                         run `owlshift resume {ticket}`."
                    ),
                    Err(why) => write!(
                        f,
                        "\nNothing was posted on the ticket ({why}): answer there, then run \
                         `owlshift do {ticket}` again; the run resumes from its plan."
                    ),
                }
            }
            Self::Waiting {
                ticket,
                round,
                decider,
                since,
            } => write!(
                f,
                "Waiting: the questions of round {round} wait for {decider}, with no new comment \
                 from them since {since}. Once they answer on the ticket, run `owlshift resume \
                 {ticket}` again."
            ),
            Self::Reasked {
                ticket,
                round,
                reask,
                open,
            } => {
                writeln!(
                    f,
                    "Asked again on the ticket (round {round}, re-ask {reask} of {MAX_REASKS}): \
                     the answers left these questions open."
                )?;
                for (question, verdict) in open {
                    writeln!(f, "{}: {}", question.id, verdict.reason)?;
                }
                write!(
                    f,
                    "Once the decider answers on the ticket, run `owlshift resume {ticket}` again."
                )
            }
            Self::CounterQuestion { ticket, asked } => {
                writeln!(f, "The decider asked back instead of answering:")?;
                for verdict in asked {
                    writeln!(f, "{}: {}", verdict.question, verdict.reason)?;
                }
                write!(
                    f,
                    "Reply on the ticket (Owlshift does not reply yet); once the decider comments \
                     again, run `owlshift resume {ticket}` to check the answers."
                )
            }
            Self::CheckFailed { ticket, detail } => write!(
                f,
                "The answer check failed: {detail}. Run `owlshift resume {ticket}` to check the \
                 same answers again; a second failure parks the ticket."
            ),
            Self::Parked {
                reason,
                detail,
                unposted,
            } => {
                write!(f, "Parked: {}: {detail}", park_reason(*reason))?;
                if *reason == ParkReason::IsolationBreach {
                    f.write_str(
                        "\nThe project is refused until a person looks: see the `unverified` \
                         file in its folder.",
                    )?;
                }
                if let Some(why) = unposted {
                    write!(
                        f,
                        "\nThe PARKED comment could not be posted on the ticket: {why}."
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

fn refused(what: &str, error: impl fmt::Display) -> Stop {
    Stop::Refused(format!("{what}: {error}"))
}

fn core_error(error: impl fmt::Debug) -> Stop {
    Stop::Refused(format!("the core state machine: {error:?}"))
}

fn nothing_to_resume(ticket: &TicketId) -> Stop {
    Stop::Refused(format!(
        "nothing to resume: no question was asked on {ticket}; run `owlshift do {ticket}`"
    ))
}

/// Whether the ticket waits for its decider's answers, parked or not.
fn awaits_input(state: &TicketState) -> bool {
    matches!(
        state.status(),
        Status::NeedsInput { .. }
            | Status::Parked {
                awaiting_input: true,
                ..
            }
    )
}

/// What the runs of one command gathered for the delivery report.
#[derive(Default)]
struct Gathered {
    gate_failure: Option<GateFailure>,
    decisions: Vec<Decision>,
    followups: Vec<Followup>,
}

/// Which command runs.
#[derive(Clone, Copy)]
enum Command {
    Do,
    Resume,
}

impl Command {
    fn name(self) -> &'static str {
        match self {
            Command::Do => "do",
            Command::Resume => "resume",
        }
    }
}

/// What a command holds once the project is its own and the ticket read.
struct Prepared {
    _lock: ProjectLock,
    ticket: TicketId,
    found: Ticket,
    /// The ticket's decider when the command started ([`pending_decider`],
    /// [`decide`]), or why it has none. `do` refuses a ticket without one;
    /// `resume` only once Build is about to run, since waiting, the answer
    /// check and a re-ask need the decider recorded on the latest ask alone.
    decider: Result<AskDecider, String>,
    base: Base,
    checkout: PathBuf,
    worktree: PathBuf,
    branch: String,
    /// The ticket ref as read, then as last written; `None` until a question
    /// round opens.
    stored: Option<Stored>,
}

impl Prepared {
    /// The asks the ticket ref keeps, none without one.
    fn questions(&self) -> TicketQuestions {
        self.stored
            .as_ref()
            .map(|stored| stored.record.questions.clone())
            .unwrap_or_default()
    }

    /// The ticket's decider when the command started, which a Build and a
    /// new round need: the refusal that says why when it has none.
    fn current(&self) -> Result<&AskDecider, Stop> {
        self.decider
            .as_ref()
            .map_err(|why| Stop::Refused(why.clone()))
    }

    /// The decider recorded on the latest ask, by name, for messages.
    fn asked_of(&self) -> Option<String> {
        self.questions()
            .latest()
            .map(|ask| name_of(&ask.decider, &self.found))
    }
}

/// What a Build needs beyond the preparation, read just before its first
/// run.
struct Dispatched {
    head: Branch,
    base_branch: Branch,
    rules: Vec<Rule>,
}

/// One run through the executor: its id, its report, and the isolation
/// breaches it was quarantined for.
struct Ran {
    run: String,
    report: RunReport,
    breaches: Vec<String>,
}

impl OnDemand<'_> {
    /// `owlshift do`: runs `ticket` to a delivered pull request, from Ready;
    /// see the module documentation. Refused while its questions wait.
    pub fn run(&self, ticket: &TicketId, sink: &mut EventSink<'_>) -> Result<Delivered, Stop> {
        let mut p = self.prepare(ticket, Command::Do)?;
        let round = match &p.stored {
            Some(stored) => {
                let state = TicketState::try_from(&stored.record.state).map_err(core_error)?;
                if awaits_input(&state) {
                    return Err(Stop::Refused(format!(
                        "the questions of round {} on {ticket} wait for {}: answer them on the \
                         ticket, then run `owlshift resume {ticket}`",
                        state.round(),
                        p.asked_of().unwrap_or_else(|| "the decider".to_owned())
                    )));
                }
                state.round()
            }
            None => 0,
        };
        // From Ready, keeping the rounds already asked: their asks stay in
        // the thread, and a new round follows them.
        let ready =
            TicketState::restore(Status::Active(Stage::Ready), round, 0, 0).map_err(core_error)?;
        let state = match ready.apply(PIPELINE, Event::Dispatched) {
            Ok(Transition::To(state)) => state,
            other => return Err(core_error(other)),
        };
        let dispatched = self.dispatch(&p, Command::Do, sink)?;
        self.build(&mut p, &dispatched, state, sink)
    }

    /// `owlshift resume`: picks `ticket` up where its ticket ref left it. A
    /// parked ticket is restarted. While questions wait, the answer check
    /// runs once the decider has answered, and the ticket resumes, its open
    /// questions are asked again, or it parks; a ticket at Build runs on to a
    /// delivery.
    pub fn resume(&self, ticket: &TicketId, sink: &mut EventSink<'_>) -> Result<Delivered, Stop> {
        let mut p = self.prepare(ticket, Command::Resume)?;
        let Some(stored) = &p.stored else {
            return Err(nothing_to_resume(ticket));
        };
        let mut state = TicketState::try_from(&stored.record.state).map_err(core_error)?;
        // A person typing `resume` on a parked ticket is the restart the
        // core waits for.
        if let Status::Parked { at, .. } = state.status() {
            state = match state.apply(PIPELINE, Event::Restarted) {
                Ok(Transition::To(state)) => state,
                other => return Err(core_error(other)),
            };
            self.keep(&mut p, &state)?;
            sink.emit(
                ticket,
                None,
                EventKind::Decision,
                data([("restarted", json!(true)), ("stage", json!(stage_name(at)))]),
            );
        }
        if let Status::NeedsInput { .. } = state.status() {
            state = self.check_answers(&mut p, state, sink)?;
        }
        if state.status() != Status::Active(Stage::Build) {
            return Err(Stop::Refused(format!(
                "nothing to resume: {ticket} is at {} and waits for no answer; run `owlshift do \
                 {ticket}` to run it again",
                stage_name(state.stage())
            )));
        }
        // Build may ask a new round, which goes to the decider now.
        p.current()?;
        let dispatched = self.dispatch(&p, Command::Resume, sink)?;
        self.build(&mut p, &dispatched, state, sink)
    }

    /// Everything before a run: the lock, the marker, the confinement, the
    /// ticket and its decider, the dedicated checkout, the kept worktree's
    /// link, and the ticket ref.
    fn prepare(&self, ticket: &TicketId, command: Command) -> Result<Prepared, Stop> {
        check_team(self.config, ticket).map_err(Stop::Refused)?;
        // The lock before the marker: every run writes the marker when it
        // starts, so while another `do` works the project the marker only
        // says a run is in flight. Once the lock is ours, a marker means a
        // run that ended, or was cut off, before its check passed.
        let lock = match self.dirs.lock() {
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
        // What the executor would refuse at the spawn is refused before
        // anything is cloned or read: a machine where agents cannot be
        // confined, or a harness with no login made for agent runs (OWL-41);
        // the message says what to install or which command to run once.
        self.executor
            .agent
            .sandbox_ready()
            .map_err(|e| Stop::Refused(e.to_string()))?;
        for harness in [self.build, self.answer_check] {
            harness
                .sandbox_needs(&self.executor.agent)
                .map_err(|e| Stop::Refused(e.to_string()))?;
        }
        // The ticket ref lives in the dedicated checkout: without one,
        // nothing was asked yet, and nothing is cloned to find that out.
        let checkout = self.dirs.checkout();
        if matches!(command, Command::Resume) && fs::symlink_metadata(&checkout).is_err() {
            return Err(nothing_to_resume(ticket));
        }
        let found = self
            .tracker
            .ticket(ticket)
            .map_err(|e| refused(&format!("reading {ticket}"), e))?;
        // `do` needs a decider: refused before anything is cloned when the
        // ticket alone says it has none.
        let pending = pending_decider(&found);
        if let (Command::Do, Err(why)) = (command, &pending) {
            return Err(Stop::Refused(why.clone()));
        }

        let git = &self.executor.git;
        let base =
            project::sync_checkout(git, self.dirs, self.remote_url).map_err(Stop::Refused)?;
        // Zone owners come from the project file at the base commit, read
        // only for a ticket without an assignee that declares zones.
        let decider = pending.and_then(|pending| match pending {
            Pending::Known(decider) => Ok(decider),
            Pending::Owners(declared) => project::zone_owners_at(git, &checkout, &base)
                .and_then(|owners| decide(&found, &declared, &owners)),
        });
        if let (Command::Do, Err(why)) = (command, &decider) {
            return Err(Stop::Refused(why.clone()));
        }
        let worktree = self.dirs.worktree(ticket);
        // A worktree kept from an earlier `do` is trusted only while its
        // `.git` still links it to the checkout. The executor refuses such a
        // worktree too; checked here, under the lock, the project is marked
        // and refused until a person looks.
        if fs::symlink_metadata(&worktree).is_ok()
            && let Err(reason) =
                crate::executor::check_worktree_link(&checkout.join(".git"), &worktree)
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
        let stored = ticket_ref::read(git, &checkout, ticket).map_err(|e| {
            let name = owlshift_contracts::refs::ticket_ref(ticket);
            Stop::Refused(format!(
                "reading the ticket's ref: {e}. To start {ticket} over, delete {name} in {} \
                 (`git update-ref -d {name}`): its comments stay on the ticket, and its rounds \
                 restart from 1",
                checkout.display()
            ))
        })?;
        Ok(Prepared {
            _lock: lock,
            ticket: ticket.clone(),
            found,
            decider,
            base,
            checkout,
            worktree,
            branch: branch_for(ticket),
            stored,
        })
    }

    /// What a Build needs, read just before its first run: the forge is asked
    /// before an hour of Build, so a refused token or an unknown repository
    /// shows now, and the project's rules come from the base commit, after
    /// the fetch, never from the ticket's branch, which agents write. Every
    /// Build run of the command gets the same rules.
    fn dispatch(
        &self,
        p: &Prepared,
        command: Command,
        sink: &mut EventSink<'_>,
    ) -> Result<Dispatched, Stop> {
        let head = Branch::new(&p.branch).map_err(|e| refused("the ticket's branch", e))?;
        let base_branch = Branch::new(&p.base.branch).map_err(|e| refused("the base branch", e))?;
        let open = self
            .forge
            .find_open_pull_request(&head, &base_branch)
            .map_err(|e| refused(&format!("GitHub ({})", self.forge.repo()), e))?;
        let rules =
            rules::project_rules(&self.executor.git, &p.checkout, &p.base, &self.config.stack)
                .map_err(|e| refused("the project's rules", e))?;
        sink.emit(
            &p.ticket,
            None,
            EventKind::Dispatch,
            data([
                ("command", json!(command.name())),
                ("title", json!(p.found.title)),
                ("branch", json!(p.branch)),
                ("base", json!(p.base.remote_ref)),
                ("base_commit", json!(p.base.commit)),
                ("worktree", path(&p.worktree)),
                ("checkout", path(&p.checkout)),
                ("pull_request", json!(open.map(|pr| pr.number))),
                (
                    "rules",
                    json!(rules.iter().map(|rule| &rule.source).collect::<Vec<_>>()),
                ),
            ]),
        );
        Ok(Dispatched {
            head,
            base_branch,
            rules,
        })
    }

    /// Runs Build from `state` until it delivers or stops: a failed run gets
    /// one more run before the core parks the ticket, questions open a round.
    fn build(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        mut state: TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let current = person(p.current()?, &p.found);
        self.keep(p, &state)?;
        let mut gathered = Gathered::default();
        let mut attempt = 0u32;
        let report = loop {
            attempt += 1;
            let comments = self.comments(&p.ticket)?;
            let brief = self.brief(
                p,
                Role::Build,
                &comments,
                &dispatched.rules,
                gathered.gate_failure.clone(),
                &current,
            );
            let ran = self.execute(p, self.executor, self.build, &brief, attempt, sink)?;
            if let Some(gate) = &ran.report.gate {
                gathered.gate_failure = gate.failure.clone();
            }
            let (event, result) = if ran.breaches.is_empty() {
                core_event(&ran.report.outcome)
            } else {
                (Event::Quarantined, None)
            };
            if let Some(result) = result {
                gather(&mut gathered.decisions, &result.decisions);
                gather(&mut gathered.followups, &result.followups);
            }
            match state.apply(PIPELINE, event) {
                Ok(Transition::To(next)) => state = next,
                Ok(Transition::Parked {
                    state: parked,
                    reason,
                }) => return Err(self.park(p, &parked, reason, &ran, None, Vec::new(), sink)),
                other => return Err(core_error(other)),
            }
            match state.status() {
                Status::Active(Stage::Build) => {
                    self.keep(p, &state)?;
                    if let Outcome::UsageLimit { resets_at } = ran.report.outcome {
                        return Err(Stop::UsageLimit { resets_at });
                    }
                }
                Status::NeedsInput { .. } => {
                    let result = result.ok_or_else(|| {
                        Stop::Refused("a question round without a result".to_owned())
                    })?;
                    return Err(self.ask(p, &state, result, &ran.run, sink));
                }
                // Build completed: in this version, delivery follows.
                _ => {
                    self.keep(p, &state)?;
                    break ran.report;
                }
            }
        };
        self.deliver(p, dispatched, report, gathered, sink)
    }

    /// Opens a question round: the Writer posts the questions, and the
    /// ticket ref keeps the ask and the state. Returns the stop that says
    /// so. A false premise with no question, or a post that failed, keeps
    /// nothing: the questions are printed.
    fn ask(
        &self,
        p: &mut Prepared,
        state: &TicketState,
        result: &RunResult,
        run: &str,
        sink: &mut EventSink<'_>,
    ) -> Stop {
        let ticket = p.ticket.clone();
        sink.emit(
            &ticket,
            Some(run),
            EventKind::Gate,
            data([
                ("opened", json!("questions")),
                ("round", json!(state.round())),
                ("status", status_name(result.status)),
                ("questions", json!(result.questions.len())),
            ]),
        );
        let stop = |posted| Stop::NeedsInput {
            ticket: ticket.clone(),
            status: result.status,
            summary: result.summary.clone(),
            questions: result.questions.clone(),
            posted,
        };
        let Some(round) = NonZeroU32::new(state.round()) else {
            return stop(Err("no question round is open".to_owned()));
        };
        if result.questions.is_empty() {
            return stop(Err("the run asked no question".to_owned()));
        }
        // The round goes to the decider this command resolved, for good.
        let decider = match &p.decider {
            Ok(decider) => decider.clone(),
            Err(why) => return stop(Err(why.clone())),
        };
        let comment = QuestionsComment {
            ticket: ticket.clone(),
            round,
            summary: result.summary.clone(),
            questions: result.questions.clone(),
            premise_false: result.status == result::Status::PremiseFalse,
        };
        let posted = match Writer::new(self.tracker).post_questions(&comment) {
            Ok(posted) => posted,
            Err(error) => return stop(Err(format!("posting them failed: {error}"))),
        };
        sink.emit(
            &ticket,
            Some(run),
            EventKind::TrackerWrite,
            comment_written("QUESTIONS", &posted),
        );
        let mut questions = p.questions();
        questions.asks.push(Ask {
            kind: AskKind::Questions,
            round,
            at: posted.created_at,
            comment: posted.id.clone(),
            questions: result.questions.clone(),
            decider,
            verdicts: Vec::new(),
        });
        if let Err(error) = self.store(p, state, questions) {
            return Stop::Refused(format!(
                "the questions of round {round} are on the ticket (comment {}), but keeping them \
                 in the ticket's ref failed: {error}; run `owlshift do {ticket}` to ask them again",
                posted.id
            ));
        }
        stop(Ok(round))
    }

    /// Runs the answer check once the decider has answered, and acts on its
    /// one core event: the ticket resumes (the state returned), its open
    /// questions are asked again, the counter-question waits for a reply, or
    /// it parks (a stop). A failed or interrupted check moves nothing the
    /// next one reads, so it is retried on the same answers.
    fn check_answers(
        &self,
        p: &mut Prepared,
        state: TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let ticket = p.ticket.clone();
        let mut questions = p.questions();
        let comments = self.comments(&ticket)?;
        let (Some(since), Some(latest)) = (questions.answers_after(), questions.latest()) else {
            return Err(Stop::Refused(format!(
                "the ticket's ref of {ticket} waits for answers but keeps no ask"
            )));
        };
        // Only the decider recorded on the ask answers it, whoever decides
        // the ticket now; a re-ask keeps its round's decider.
        let asked = latest.decider.clone();
        let asked_of = person(&asked, &p.found);
        if answer_check::new_answer(&comments, &asked_of, &questions).is_none() {
            return Err(Stop::Waiting {
                ticket,
                round: state.round(),
                decider: name_of(&asked, &p.found),
                since,
            });
        }
        let read_through = answer_check::newest_decider_edit(&comments, &asked_of);
        // The ticket's author is judged against the decider now, or the
        // asked one when the ticket has none now: the description is
        // context for the answer check either way.
        let current = p
            .decider
            .as_ref()
            .map_or(asked_of, |decider| person(decider, &p.found));
        let brief = self.brief(p, Role::AnswerCheck, &comments, &[], None, &current);
        let executor = Executor {
            timeout: ANSWER_CHECK_TIMEOUT,
            ..self.executor.clone()
        };
        let ran = self.execute(p, &executor, self.answer_check, &brief, 1, sink)?;
        let (event, result) = if ran.breaches.is_empty() {
            answer_check::event(&ran.report.outcome)
        } else {
            (Event::Quarantined, None)
        };
        let verdicts: Vec<AnswerVerdict> = match (event, result) {
            (Event::Answered | Event::Incomplete | Event::CounterQuestion, Some(result)) => {
                // The answers this check read are judged: only a newer
                // comment of the decider is a new answer. Its verdicts are
                // kept with the ask they judge, for the round's RESUME.
                questions.checked_through = read_through.max(questions.checked_through);
                if let Some(ask) = questions.asks.last_mut() {
                    ask.verdicts.clone_from(&result.verdicts);
                }
                result.verdicts.clone()
            }
            _ => Vec::new(),
        };
        let next = match state.apply(PIPELINE, event) {
            Ok(Transition::To(next)) => next,
            Ok(Transition::Parked {
                state: parked,
                reason,
            }) => {
                let open = if reason == ParkReason::Reasks {
                    answer_check::open_questions(&brief, &verdicts)
                } else {
                    Vec::new()
                };
                return Err(self.park(p, &parked, reason, &ran, Some(questions), open, sink));
            }
            other => return Err(core_error(other)),
        };
        let checked = |outcome: &str| {
            data([
                ("answer_check", json!(outcome)),
                ("round", json!(next.round())),
                ("reasks", json!(next.reasks())),
                ("verdicts", verdict_classes(&verdicts)),
            ])
        };
        match event {
            Event::Answered => {
                // RESUME before the state is kept, as a RE-ASK: when the
                // post fails, nothing of this check is kept and the next
                // `resume` checks the same answers again.
                let round = NonZeroU32::new(next.round())
                    .ok_or_else(|| Stop::Refused("no question round is open".to_owned()))?;
                let resume = ResumeComment {
                    ticket: ticket.clone(),
                    round,
                    understood: answer_check::understood(
                        questions
                            .asks
                            .iter()
                            .filter(|ask| ask.round == round)
                            .map(|ask| (&ask.questions[..], &ask.verdicts[..])),
                    ),
                };
                let posted = Writer::new(self.tracker)
                    .post_resume(&resume)
                    .map_err(|e| refused("posting the resume on the ticket", e))?;
                sink.emit(
                    &ticket,
                    Some(&ran.run),
                    EventKind::TrackerWrite,
                    comment_written("RESUME", &posted),
                );
                self.store(p, &next, questions).map_err(|e| {
                    Stop::Refused(format!(
                        "the resume is on the ticket (comment {}), but keeping the answered state \
                         in the ticket's ref failed: {e}",
                        posted.id
                    ))
                })?;
                sink.emit(
                    &ticket,
                    Some(&ran.run),
                    EventKind::Gate,
                    checked("answered"),
                );
                Ok(next)
            }
            Event::Incomplete => {
                let round = NonZeroU32::new(next.round())
                    .ok_or_else(|| Stop::Refused("no question round is open".to_owned()))?;
                let open = answer_check::open_questions(&brief, &verdicts);
                if open.is_empty() {
                    return Err(Stop::Refused(
                        "the answer check found the answers incomplete but no question open"
                            .to_owned(),
                    ));
                }
                let reask = ReaskComment {
                    ticket: ticket.clone(),
                    round,
                    reask: next.reasks(),
                    open: open.clone(),
                };
                let posted = Writer::new(self.tracker)
                    .post_reask(&reask)
                    .map_err(|e| refused("posting the re-ask on the ticket", e))?;
                sink.emit(
                    &ticket,
                    Some(&ran.run),
                    EventKind::TrackerWrite,
                    comment_written("RE-ASK", &posted),
                );
                questions.asks.push(Ask {
                    kind: AskKind::Reask,
                    round,
                    at: posted.created_at,
                    comment: posted.id.clone(),
                    questions: open.iter().map(|(question, _)| question.clone()).collect(),
                    decider: asked,
                    verdicts: Vec::new(),
                });
                self.store(p, &next, questions).map_err(|e| {
                    Stop::Refused(format!(
                        "the re-ask is on the ticket (comment {}), but keeping it in the \
                         ticket's ref failed: {e}",
                        posted.id
                    ))
                })?;
                sink.emit(
                    &ticket,
                    Some(&ran.run),
                    EventKind::Gate,
                    checked("incomplete"),
                );
                Err(Stop::Reasked {
                    ticket,
                    round,
                    reask: next.reasks(),
                    open,
                })
            }
            Event::CounterQuestion => {
                self.store(p, &next, questions)
                    .map_err(|e| refused("keeping the ticket's state in its ref", e))?;
                sink.emit(
                    &ticket,
                    Some(&ran.run),
                    EventKind::Gate,
                    checked("counter_question"),
                );
                Err(Stop::CounterQuestion {
                    ticket,
                    asked: verdicts
                        .into_iter()
                        .filter(|verdict| verdict.class == AnswerClass::CounterQuestion)
                        .collect(),
                })
            }
            Event::RunFailed => {
                self.store(p, &next, questions)
                    .map_err(|e| refused("keeping the ticket's state in its ref", e))?;
                Err(Stop::CheckFailed {
                    ticket,
                    detail: park_detail(&ran.report.outcome),
                })
            }
            Event::Interrupted => match ran.report.outcome {
                Outcome::UsageLimit { resets_at } => Err(Stop::UsageLimit { resets_at }),
                _ => Err(core_error(event)),
            },
            other => Err(core_error(other)),
        }
    }

    /// Parks the ticket: keeps its parked state when it has a ticket ref,
    /// with `questions` when the answer check changed them, posts the PARKED
    /// comment, records the decision, and returns the stop that says why.
    /// `open` holds the questions still open at the re-ask limit.
    ///
    /// The comment is posted even when keeping the state failed, as when a
    /// run that broke isolation moved the ticket ref: the ticket stopped
    /// either way, and the person learns it on the ticket. A post that
    /// failed leaves the park as it is and is said in the stop.
    #[allow(clippy::too_many_arguments)]
    fn park(
        &self,
        p: &mut Prepared,
        parked: &TicketState,
        reason: ParkReason,
        ran: &Ran,
        questions: Option<TicketQuestions>,
        open: Vec<(Question, AnswerVerdict)>,
        sink: &mut EventSink<'_>,
    ) -> Stop {
        let detail = if ran.breaches.is_empty() {
            park_detail(&ran.report.outcome)
        } else {
            ran.breaches.join("; ")
        };
        // A ticket without a ref keeps nothing: a ref is made only once a
        // question round opens.
        let questions = questions.or_else(|| p.stored.as_ref().map(|_| p.questions()));
        let kept = match questions {
            Some(questions) => self
                .store(p, parked, questions)
                .map_err(|e| format!("keeping the ticket's state in its ref failed: {e}")),
            None => Ok(()),
        };
        let comment = ParkedComment {
            ticket: p.ticket.clone(),
            reason,
            // A breach's details describe this machine: they stay here.
            detail: if reason == ParkReason::IsolationBreach {
                String::new()
            } else {
                detail.clone()
            },
            round: NonZeroU32::new(parked.round()).filter(|_| reason == ParkReason::Reasks),
            open,
            restart: if p.stored.is_some() {
                Restart::Resume
            } else {
                Restart::Do
            },
        };
        let unposted = match Writer::new(self.tracker).post_parked(&comment) {
            Ok(posted) => {
                sink.emit(
                    &p.ticket,
                    Some(&ran.run),
                    EventKind::TrackerWrite,
                    comment_written("PARKED", &posted),
                );
                None
            }
            Err(error) => Some(error.to_string()),
        };
        sink.emit(
            &p.ticket,
            Some(&ran.run),
            EventKind::Decision,
            data([
                ("parked", json!(format!("{reason:?}"))),
                ("detail", json!(detail)),
            ]),
        );
        if let Err(why) = kept {
            let posted = match &unposted {
                None => "the PARKED comment is on the ticket".to_owned(),
                Some(error) => format!("the PARKED comment could not be posted: {error}"),
            };
            return Stop::Refused(format!(
                "{} parked ({}: {detail}), but {why}; {posted}",
                p.ticket,
                park_reason(reason)
            ));
        }
        Stop::Parked {
            reason,
            detail,
            unposted,
        }
    }

    /// Keeps `state` in the ticket's ref, when the ticket has one: a ref is
    /// made only once a question round opens.
    fn keep(&self, p: &mut Prepared, state: &TicketState) -> Result<(), Stop> {
        if p.stored.is_none() {
            return Ok(());
        }
        let questions = p.questions();
        self.store(p, state, questions)
            .map_err(|e| refused("keeping the ticket's state in its ref", e))
    }

    /// Writes `state` and `questions` as the ticket's ref, unless it already
    /// holds them.
    fn store(
        &self,
        p: &mut Prepared,
        state: &TicketState,
        questions: TicketQuestions,
    ) -> Result<(), String> {
        let record = TicketRecord {
            state: PersistedState::from(state),
            questions,
        };
        if p.stored
            .as_ref()
            .is_some_and(|stored| stored.record == record)
        {
            return Ok(());
        }
        let previous = p.stored.as_ref().map(|stored| stored.commit.as_str());
        let commit = ticket_ref::write(
            &self.executor.git,
            &p.checkout,
            &p.ticket,
            &record,
            previous,
        )?;
        p.stored = Some(Stored { record, commit });
        Ok(())
    }

    /// Runs one role through the executor, between the project's marker
    /// and the events: `run_started`, `run_ended`, and `usage` when the
    /// harness reported it.
    fn execute(
        &self,
        p: &Prepared,
        executor: &Executor,
        harness: &dyn Harness,
        brief: &Brief,
        attempt: u32,
        sink: &mut EventSink<'_>,
    ) -> Result<Ran, Stop> {
        let ticket = &p.ticket;
        let (run, run_dir) =
            new_run_dir(&self.dirs.runs(ticket)).map_err(|e| refused("the run directory", e))?;
        sink.emit(
            ticket,
            Some(&run),
            EventKind::RunStarted,
            data([
                ("role", json!(brief.role.as_str())),
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
            main: &p.checkout,
            worktree: &p.worktree,
            branch: &p.branch,
            // The commit resolved after the fetch, not the name: a run can
            // move a remote-tracking ref (OWL-51).
            base: &p.base.commit,
            run_dir: &run_dir,
            brief,
        };
        let report = match executor.run(&spec, harness) {
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
        // The executor's isolation check starts with the worktree's `.git`
        // link: a redirected one is among the violations.
        let breaches: Vec<String> = match &report.outcome {
            Outcome::Quarantined(violations) => {
                violations.iter().map(ToString::to_string).collect()
            }
            _ => Vec::new(),
        };
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
        sink.emit(ticket, Some(&run), EventKind::RunEnded, run_ended(&report));
        if let Some(usage) = &report.usage {
            sink.emit(ticket, Some(&run), EventKind::Usage, usage_data(usage));
        }
        Ok(Ran {
            run,
            report,
            breaches,
        })
    }

    fn comments(&self, ticket: &TicketId) -> Result<Vec<Comment>, Stop> {
        self.tracker
            .comments(ticket)
            .map_err(|e| refused(&format!("reading the comments of {ticket}"), e))
    }

    /// The brief of one run of `role`: Build writes in the worktree and
    /// resumes from the plan it left; the answer check only reads, and gets
    /// no rule, its context kept to the ticket, the questions and the
    /// answers (architecture section 9). `current` is the ticket's decider
    /// now, which the ticket's author and the comments before any ask are
    /// judged against ([`thread`]); the brief's `decider` is the one in
    /// force at the end of the thread.
    fn brief(
        &self,
        p: &Prepared,
        role: Role,
        comments: &[Comment],
        rules: &[Rule],
        gate_failure: Option<GateFailure>,
        current: &Person,
    ) -> Brief {
        let build = role == Role::Build;
        let asks = p.questions().asks;
        let in_force = asks.last().map_or_else(
            || current.name.clone(),
            |ask| name_of(&ask.decider, &p.found),
        );
        Brief {
            format: Format,
            role,
            project: self.forge.repo().to_string(),
            ticket: TicketBrief {
                id: p.found.id.clone(),
                title: p.found.title.clone(),
                url: None,
                labels: p.found.labels.clone(),
                author: account_author(&p.found.author, current),
                description: p.found.description.clone(),
            },
            decider: in_force,
            thread: thread(comments, &asks, current),
            checkpoint: if build { checkpoint(&p.worktree) } else { None },
            zones: brief_zones(p.found.labels.iter().map(String::as_str)),
            resources: Vec::new(),
            rules: rules.to_vec(),
            permissions: Permissions {
                level: if build {
                    PermissionLevel::WriteWorktree
                } else {
                    PermissionLevel::ReadOnly
                },
                network: false,
                browser: false,
            },
            gate: self.config.stack.gate.clone(),
            gate_failure,
            result_path: RelativePath::new(RESULT_PATH).expect("RESULT_PATH is a relative path"),
        }
    }

    /// Delivers a Build `done` whose gate passed. The push leaves from the
    /// dedicated checkout, never from the worktree (`Writer::push_branch`).
    fn deliver(
        &self,
        p: &Prepared,
        dispatched: &Dispatched,
        report: RunReport,
        gathered: Gathered,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let (ticket, head, base) = (&p.ticket, &dispatched.head, &dispatched.base_branch);
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

        let pushed = writer
            .push_branch(&self.executor.git, &p.checkout, "origin", &commit, head)
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
            None => (p.found.title.clone(), result.summary.clone()),
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

/// A brief's thread: each comment as [`comment_author`] marks it, but for
/// the runner's own comments that posted an ask, whose place the ask takes
/// as a `questions` or `reask` entry. An ask goes after the comments of its
/// time or earlier, so one whose comment is gone from the tracker still
/// takes its place; a comment posted in the same second as an ask reads as
/// before it.
///
/// A comment is the decider's when its author is the decider recorded on
/// the latest ask before it, or, before any ask, `current`, the ticket's
/// decider now (build plan, "Who answers which ask"): the answers a check
/// accepted stay instructions for the Build they unblock, and a person who
/// became the decider after an ask cannot answer it.
pub(crate) fn thread(comments: &[Comment], asks: &[Ask], current: &Person) -> Vec<ThreadEntry> {
    let posted: HashSet<&str> = asks.iter().map(|ask| ask.comment.as_str()).collect();
    let mut asks = asks.iter().peekable();
    let mut thread = Vec::new();
    let mut in_force = current.clone();
    for comment in comments {
        if posted.contains(comment.id.as_str()) {
            continue;
        }
        while let Some(ask) = asks.next_if(|ask| ask.at < comment.created_at) {
            // Only the account is matched, never a name.
            in_force = Person {
                id: ask.decider.account.clone(),
                name: String::new(),
            };
            thread.push(ask.entry());
        }
        thread.push(ThreadEntry::Comment {
            at: comment.created_at,
            author: comment_author(comment, &in_force),
            body: comment.body.clone(),
        });
    }
    thread.extend(asks.map(Ask::entry));
    thread
}

/// Who decides a ticket, as far as the ticket alone tells.
enum Pending {
    /// Its decider: the assignee.
    Known(AskDecider),
    /// No assignee: the one owner of these declared zones decides, by the
    /// owners of the project file at the base commit.
    Owners(Vec<Resource>),
}

/// The first half of the decider's resolution (architecture section 4),
/// which needs no project file: the assignee when the ticket has one; else
/// the zones its `zone:` labels declare, whose owners decide. A malformed
/// `zone:` label, and a ticket with neither an assignee nor a declared zone,
/// which no owner could change, have no decider: the reason.
fn pending_decider(ticket: &Ticket) -> Result<Pending, String> {
    let assignee = ticket.assignee.as_ref().map(|person| person.id.as_str());
    // Labels play no part for a ticket with an assignee.
    if let Ok(found) = decider::decider(assignee, &[], &ZoneOwners::default()) {
        return Ok(Pending::Known(recorded(found)));
    }
    let declared = declared_zones(ticket.labels.iter().map(String::as_str))
        .map_err(|error| format!("{} has no decider: {error}", ticket.id))?;
    if declared.is_empty() {
        return decide(ticket, &declared, &ZoneOwners::default()).map(Pending::Known);
    }
    Ok(Pending::Owners(declared))
}

/// The decider of `ticket` with `declared` zones and the zone `owners` of
/// the project file at the base commit, or why it has none and how to give
/// it one.
fn decide(
    ticket: &Ticket,
    declared: &[Resource],
    owners: &ZoneOwners,
) -> Result<AskDecider, String> {
    let assignee = ticket.assignee.as_ref().map(|person| person.id.as_str());
    decider::decider(assignee, declared, owners)
        .map(recorded)
        .map_err(|why| {
            let hint = match why {
                NoDecider::NoOwnedZone => {
                    "; or label it `zone:<folder>` for each zone it touches, which `[zones]` in \
                     owlshift.toml on the forge's default branch gives an owner"
                }
                NoDecider::SeveralOwners(_) => "",
            };
            format!("{} has no decider: {why}{hint}", ticket.id)
        })
}

/// A decider as an ask records it.
fn recorded(found: Decider<'_>) -> AskDecider {
    AskDecider {
        account: found.account().to_owned(),
        by: found.rule(),
    }
}

/// A recorded decider as the tracker's person, named by [`name_of`].
fn person(decider: &AskDecider, ticket: &Ticket) -> Person {
    Person {
        id: decider.account.clone(),
        name: name_of(decider, ticket),
    }
}

/// A recorded decider's name: the assignee's when it is their account,
/// else the account itself, the one way a zone owner is known until the
/// identity map (P10).
fn name_of(decider: &AskDecider, ticket: &Ticket) -> String {
    ticket
        .assignee
        .as_ref()
        .filter(|assignee| assignee.id == decider.account)
        .map_or_else(|| decider.account.clone(), |assignee| assignee.name.clone())
}

/// The `tracker_write` event of a marked comment the runner posted.
fn comment_written(kind: &str, comment: &Comment) -> Data {
    data([
        ("target", json!("tracker")),
        ("action", json!("comment")),
        ("kind", json!(kind)),
        ("comment", json!(comment.id)),
    ])
}

/// Each verdict's question and class, for an event; the reasons stay in the
/// run's result.
fn verdict_classes(verdicts: &[AnswerVerdict]) -> Value {
    json!(
        verdicts
            .iter()
            .map(|verdict| json!({ "question": verdict.question.as_str(), "class": verdict.class }))
            .collect::<Vec<_>>()
    )
}

/// A stage's name as the contracts write it, such as `build`.
fn stage_name(stage: Stage) -> String {
    serde_json::to_value(stage)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// A comment's author as the brief shows it: the runner's own marked
/// comments are Owlshift's, whoever posted them (the key's account posts
/// them on Linear); the decider is the assignee's account; anyone else is
/// quoted as data. A marker only ever demotes: the decider's text that
/// looks like one reads as data.
pub(crate) fn comment_author(comment: &Comment, decider: &Person) -> Author {
    let author = account_author(&comment.author, decider);
    if matches!(MarkedComment::parse(&comment.body), Ok(Some(_))) {
        Author {
            relation: Relation::Owlshift,
            ..author
        }
    } else {
        author
    }
}

/// A ticket's or a comment's author as the brief shows it: the decider when
/// it is the assignee's account, matched by its identifier and never its name
/// (an empty identifier matches nothing), so the text reads as instructions;
/// anyone else, or an author the tracker cannot name, is quoted as data. For
/// a ticket, the author is its creator: an edit of the description by
/// someone else after its creation is not seen.
fn account_author(author: &TrackerAuthor, decider: &Person) -> Author {
    match author {
        TrackerAuthor::Account(person) => Author {
            name: person.name.clone(),
            relation: if !person.id.trim().is_empty() && person.id == decider.id {
                Relation::Decider
            } else {
                Relation::Other
            },
        },
        TrackerAuthor::Other { name } => Author {
            name: name.clone(),
            relation: Relation::Other,
        },
    }
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
    use owlshift_core::decider::DeciderRule;

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
            (
                "https://example.invalid/@github.com/o/r.git",
                "not a github.com repository",
            ),
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

    /// Each ask takes the place of the comment that posted it; one whose
    /// comment is gone still takes its place by time, and a comment of the
    /// same second as an ask reads before it.
    #[test]
    fn each_ask_takes_the_place_of_its_comment_in_the_thread() {
        let decider = Person {
            id: "u1".into(),
            name: "Maintainer".into(),
        };
        let at = |minute: u32| -> Timestamp {
            format!("2026-10-02T10:{minute:02}:00Z").parse().unwrap()
        };
        let comment = |id: &str, minute: u32, body: &str| Comment {
            id: id.into(),
            author: TrackerAuthor::Account(decider.clone()),
            created_at: at(minute),
            edited_at: None,
            body: body.into(),
        };
        let ask = |kind, round, minute, id: &str| Ask {
            kind,
            round: NonZeroU32::new(round).unwrap(),
            at: at(minute),
            comment: id.into(),
            questions: vec![Question {
                id: owlshift_contracts::ids::QuestionId::new("Q1").unwrap(),
                category: "scope".into(),
                context: "c".into(),
                text: "t".into(),
                options: Vec::new(),
                recommendation: None,
            }],
            decider: AskDecider {
                account: "u1".into(),
                by: DeciderRule::Assignee,
            },
            verdicts: Vec::new(),
        };
        let asked = "[owlshift] QUESTIONS · round 1\n\nThe questions.\n";
        let comments = [
            comment("q1", 10, asked),
            comment("a1", 11, "Q1: half."),
            comment("same", 12, "Q1: in the re-ask's second."),
            comment("r1", 12, "[owlshift] RE-ASK · round 1\n\nAgain.\n"),
            comment("a2", 13, "Q1: all."),
            comment("d1", 14, "[owlshift] DELIVERY\n\nDone.\n"),
        ];
        let asks = [
            ask(AskKind::Questions, 1, 10, "q1"),
            ask(AskKind::Reask, 1, 12, "r1"),
            // Its comment was deleted on the tracker.
            ask(AskKind::Questions, 2, 13, "gone"),
        ];
        let entries: Vec<String> = thread(&comments, &asks, &decider)
            .iter()
            .map(|entry| match entry {
                ThreadEntry::Questions { round, .. } => format!("questions {round}"),
                ThreadEntry::Reask { round, .. } => format!("reask {round}"),
                ThreadEntry::Comment { author, body, .. } => {
                    format!("{:?}: {}", author.relation, body.lines().next().unwrap())
                }
            })
            .collect();
        assert_eq!(
            entries,
            [
                "questions 1",
                "Decider: Q1: half.",
                "Decider: Q1: in the re-ask's second.",
                "reask 1",
                "Decider: Q1: all.",
                "questions 2",
                "Owlshift: [owlshift] DELIVERY",
            ]
        );
    }

    /// Before any ask the ticket's decider now speaks; after an ask, the
    /// decider recorded on it, whoever decides the ticket now.
    #[test]
    fn a_comment_is_the_deciders_of_the_latest_ask_before_it() {
        let account = |id: &str| Person {
            id: id.into(),
            name: format!("name of {id}"),
        };
        let at = |minute: u32| -> Timestamp {
            format!("2026-10-02T10:{minute:02}:00Z").parse().unwrap()
        };
        let comment = |id: &str, by: &str, minute: u32| Comment {
            id: id.into(),
            author: TrackerAuthor::Account(account(by)),
            created_at: at(minute),
            edited_at: None,
            body: format!("{by} at {minute}"),
        };
        let ask = |round: u32, minute: u32, by: &str| Ask {
            kind: AskKind::Questions,
            round: NonZeroU32::new(round).unwrap(),
            at: at(minute),
            comment: format!("ask-{round}"),
            questions: vec![Question {
                id: owlshift_contracts::ids::QuestionId::new("Q1").unwrap(),
                category: "scope".into(),
                context: "c".into(),
                text: "t".into(),
                options: Vec::new(),
                recommendation: None,
            }],
            decider: AskDecider {
                account: by.into(),
                by: DeciderRule::ZoneOwner,
            },
            verdicts: Vec::new(),
        };
        // Round 1 went to the zone owner `bob`, round 2 to `tia`; the
        // ticket was since assigned to `ann`.
        let comments = [
            comment("c1", "ann", 1),
            comment("c2", "bob", 2),
            comment("c3", "bob", 11),
            comment("c4", "ann", 12),
            comment("c5", "bob", 21),
            comment("c6", "tia", 22),
        ];
        let asks = [ask(1, 10, "bob"), ask(2, 20, "tia")];
        let marked: Vec<String> = thread(&comments, &asks, &account("ann"))
            .iter()
            .map(|entry| match entry {
                ThreadEntry::Comment { author, body, .. } => {
                    format!("{body}: {:?}", author.relation)
                }
                _ => "ask".to_owned(),
            })
            .collect();
        assert_eq!(
            marked,
            [
                "ann at 1: Decider",
                "bob at 2: Other",
                "ask",
                "bob at 11: Decider",
                "ann at 12: Other",
                "ask",
                "bob at 21: Other",
                "tia at 22: Decider",
            ]
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

    /// The thread test above pins the account match, shared by tickets and
    /// comments; an author the tracker cannot name and an empty identifier,
    /// which the Markdown front matter accepts, never make the decider.
    #[test]
    fn an_unnamed_or_blank_author_is_never_the_decider() {
        let decider = Person {
            id: "u1".into(),
            name: "Maintainer".into(),
        };
        let unknown = account_author(&TrackerAuthor::unknown(), &decider);
        assert_eq!(unknown.relation, Relation::Other);
        let nobody = Person {
            id: String::new(),
            name: String::new(),
        };
        let blank = account_author(&TrackerAuthor::Account(nobody.clone()), &nobody);
        assert_eq!(blank.relation, Relation::Other);
    }

    /// OWL-80 as Linear gave it on 2026-09-29 (OWL-74), created by an agent
    /// through the assignee's personal API key: the assignee's account as
    /// creator, no bot actor, no external user. OWL-82, typed by hand in
    /// Linear's app, answered alike. Its description is not the decider's
    /// instructions.
    #[test]
    fn a_linear_ticket_created_through_the_deciders_key_is_not_the_deciders() {
        use owlshift_adapters::tracker::linear::{LinearTracker, Response, Transport};

        struct Owl80;
        impl Transport for Owl80 {
            fn send(&self, _: &Value) -> Result<Response, String> {
                let body = r#"{"data":{"issue":{"identifier":"OWL-80","title":"t",
                    "description":"d","priority":3,
                    "assignee":{"id":"u1","displayName":"person-1"},
                    "creator":{"id":"u1","displayName":"person-1"},
                    "botActor":null,"externalUserCreator":null,
                    "labels":{"nodes":[{"name":"Improvement"}],"pageInfo":{"hasNextPage":false}}}}}"#;
                Ok(Response {
                    status: 200,
                    body: body.to_owned(),
                })
            }
        }
        let ticket = LinearTracker::with_transport(Owl80)
            .ticket(&TicketId::new("OWL-80").unwrap())
            .unwrap();
        let decider = ticket.assignee.clone().unwrap();
        assert_eq!(
            account_author(&ticket.author, &decider).relation,
            Relation::Other
        );
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
