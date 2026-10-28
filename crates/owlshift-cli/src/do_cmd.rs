//! `owlshift do TICKET`, `owlshift continue TICKET` and `owlshift watch`:
//! open what a run needs, in this order, and hand it to
//! `owlshift_runner::on_demand`, or to `owlshift_runner::watch`, which
//! continues the tickets whose questions wait until it is asked to stop
//! (OWL-161: by degrees, architecture section 5, "Three ways to stop"). What any of them
//! can refuse without a credential, a host where agent runs cannot be
//! confined included, is refused before the keychain is opened.
//!
//! `owlshift watch --drain` runs each step in a child, this binary with
//! `continue TICKET --step ENVELOPE` (OWL-399), which opens what a run needs
//! as `continue` does and hands the step to `owlshift_runner::step::child`.
//!
//! A served project's watch, `owlshift watch --drain --served OWNER/REPO`,
//! which the service starts without a working directory, and its steps run
//! on the project file kept from its default branch (OWL-434,
//! `owlshift_runner::served`), never on a working tree's.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_adapters::notifier::Notifier;
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_adapters::tracker::{Capability, Tracker};
use owlshift_contracts::config::{
    DEFAULT_CONCURRENT_RUNS, Models, PersonalConfig, ProjectConfig, TrackerKind,
};
use owlshift_contracts::format::{Format, strip_role_front_matter};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::service::WatchRecord;
use owlshift_contracts::step::StepEnvelope;
use owlshift_contracts::{Harness, Role, Tier};
use owlshift_platform::awake::KeepAwake;
use owlshift_platform::keychain::{Keychain, KeychainError};
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::codex_machine::codex_critic;
use owlshift_runner::config::{Effective, FileState, OWLSHIFT_VERSION};
use owlshift_runner::daemon::UNSERVABLE_EXIT;
use owlshift_runner::events::{EventLog, EventSink, printable};
use owlshift_runner::executor::CodexHarness;
use owlshift_runner::executor::codex::user_login;
use owlshift_runner::executor::harness::{CLAUDE_AGENT_ACCOUNT, ClaudeHarness};
use owlshift_runner::notify::{self, DesktopNotifier};
use owlshift_runner::on_demand::{self, CodexReviewer, Delivered, OnDemand, Stop};
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::refusal::Refusal;
use owlshift_runner::roles::{
    ANSWER_CHECK_ROLE, ARBITER_ROLE, BUILD_ROLE, DESIGN_REVIEW_ROLE, DESIGN_ROLE, INTAKE_ROLE,
    REBASE_ROLE, RESOLVER_ROLE, VERIFY_ROLE,
};
use owlshift_runner::routing::{ClaudeRouter, CodexRouter, claude_model};
use owlshift_runner::served::{self, Between, Served, ServedLock};
use owlshift_runner::slots::MachineSlots;
use owlshift_runner::step::{self, Children};
use owlshift_runner::system::System;
use owlshift_runner::usage::UsageStore;
use owlshift_runner::watch::{POLL_INTERVAL, Watch, drain_refusal, presence};
use owlshift_runner::{forge, secrets, tracker};

use crate::{fail, own_build};

/// How long to wait between two reads of a pull request's head after a
/// push.
const HEAD_WAIT: Duration = Duration::from_secs(2);

/// Which command runs: `owlshift do` from Ready, `owlshift continue` from
/// where the ticket's ref left it, each on its ticket, or `owlshift watch`
/// over the project's tickets whose questions wait, also draining the ready
/// column with `--drain` (OWL-250); or a step of `--drain`'s, `owlshift
/// continue` with the step's envelope (OWL-399); or `owlshift continue
/// --to-watch`, which leaves its ticket to watch (OWL-420).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode<'a> {
    Do(&'a str),
    Continue(&'a str),
    ToWatch(&'a str),
    Watch,
    Drain,
    Step(&'a str, &'a str),
}

impl<'a> Mode<'a> {
    fn command(self) -> &'static str {
        match self {
            Mode::Do(_) => "`owlshift do`",
            Mode::Continue(_) | Mode::Step(..) => "`owlshift continue`",
            Mode::ToWatch(_) => "`owlshift continue --to-watch`",
            Mode::Watch => "`owlshift watch`",
            Mode::Drain => "`owlshift watch --drain`",
        }
    }

    /// The ticket a command names; `watch` names none.
    fn ticket(self) -> Option<&'a str> {
        match self {
            Mode::Do(ticket)
            | Mode::Continue(ticket)
            | Mode::ToWatch(ticket)
            | Mode::Step(ticket, _) => Some(ticket),
            Mode::Watch | Mode::Drain => None,
        }
    }
}

/// Runs `mode` on the project of the working directory, or, with `served`,
/// on that served project (OWL-434).
pub fn run(
    system: &dyn System,
    config: &Effective,
    mode: Mode<'_>,
    served: Option<&Repo>,
) -> ExitCode {
    let ran = match served {
        None => crate::data_dir()
            .map_err(Refusal::from)
            .and_then(|data_dir| run_with(system, config, mode, Keychain::system, None, &data_dir))
            .map(Ended::code),
        Some(repo) => run_served(system, config, mode, repo, Keychain::system),
    };
    ran.unwrap_or_else(|refusal| {
        fail(&refusal.message);
        refused_code(&refusal, mode, served.is_some())
    })
}

/// The exit code of `refusal` (OWL-435): [`UNSERVABLE_EXIT`] for a served
/// watch that no restart mends, which the daemon reads to pause its project
/// rather than start the watch again; a failure otherwise.
fn refused_code(refusal: &Refusal, mode: Mode<'_>, served: bool) -> ExitCode {
    if served && mode == Mode::Drain && refusal.lasting {
        ExitCode::from(UNSERVABLE_EXIT)
    } else {
        ExitCode::FAILURE
    }
}

/// How [`run_with`] ended.
enum Ended {
    /// With this exit code.
    Code(ExitCode),
    /// A served watch whose default branch holds a newer project file,
    /// once its steps under way ended: started again on it (OWL-434).
    Reload,
}

impl Ended {
    fn code(self) -> ExitCode {
        match self {
            Self::Code(code) => code,
            Self::Reload => ExitCode::SUCCESS,
        }
    }
}

/// `mode` on the served project `repo`, located from the list of served
/// projects, never from the working directory, whose configuration
/// (`config`) gives only the personal file. A watch reads its project file
/// from the default branch first, and again before it starts on a newer
/// one; a step runs on the one its watch kept.
fn run_served(
    system: &dyn System,
    config: &Effective,
    mode: Mode<'_>,
    repo: &Repo,
    open_keychain: fn() -> Result<Keychain, KeychainError>,
) -> Result<ExitCode, Refusal> {
    let data_dir = crate::data_dir()?;
    let git = project::runner_git();
    let served = Served::locate(&git, &data_dir, repo)?;
    let personal = owlshift_platform::paths::personal_config_file();
    let accept = |text: &str| served::accept(repo, text, personal.clone());
    let desktop = notify::desktop_enabled(&config.personal)
        .then(|| DesktopNotifier::for_this_machine(system))
        .flatten();
    let desktop = desktop.as_ref().map(|d| d as &dyn Notifier);
    loop {
        if mode == Mode::Drain {
            let mut out = io::stdout();
            let mut sink = EventSink::new(repo.to_string(), EventLog::in_dir(&data_dir), &mut out);
            served::prepare(&git, &served, &accept, desktop, &mut sink)?;
        }
        let effective = served::effective(repo, &served.dirs, personal.clone())?;
        match run_with(
            system,
            &effective,
            mode,
            open_keychain,
            Some(&served),
            &data_dir,
        )? {
            Ended::Code(code) => return Ok(code),
            Ended::Reload => {}
        }
    }
}

/// [`run`], with the keychain opened by `open_keychain`, on the served
/// project `served` when there is one, Owlshift's data directory being
/// `data_dir`: a refusal before the run is returned, for the caller to
/// print, lasting unless it says otherwise (OWL-435).
fn run_with(
    system: &dyn System,
    config: &Effective,
    mode: Mode<'_>,
    open_keychain: fn() -> Result<Keychain, KeychainError>,
    served: Option<&Served>,
    data_dir: &Path,
) -> Result<Ended, Refusal> {
    let ticket = mode
        .ticket()
        .map(TicketId::new)
        .transpose()
        .map_err(|error| error.to_string())?;
    let envelope = match (mode, &ticket) {
        (Mode::Step(_, text), Some(ticket)) => Some(envelope_of(text, ticket)?),
        _ => None,
    };
    let (project_file, project) = loaded_project(config, mode.command())?;
    if mode == Mode::Drain
        && let Some(refusal) = drain_refusal(project)
    {
        return Err(refusal.into());
    }
    if let Some(ticket) = &ticket {
        on_demand::check_team(project, ticket)?;
    }
    let git = project::runner_git();
    // A served project's checkout only locates it (OWL-434): its project
    // file is the one kept from the default branch.
    let (root, remote_url, repo) = match served {
        Some(served) => (
            served.checkout.as_path(),
            served.remote_url.clone(),
            served.repo.clone(),
        ),
        None => {
            let root = project_file.parent().unwrap_or(project_file);
            let remote_url = project::origin_url(&git, root)?;
            let repo = on_demand::check_origin(&remote_url)?;
            (root, remote_url, repo)
        }
    };
    // A watch typed in a terminal shares its project's served lock for its
    // life, so it never runs on a project the background service serves,
    // and the service waits for it to end (OWL-435): first, needing no
    // credential, so a served project is said whatever the host.
    let _served_share = match (served, mode) {
        (None, Mode::Watch | Mode::Drain) => share_served_lock(data_dir, &repo)?,
        _ => None,
    };
    // OWL-98: a host where agent runs cannot be confined is refused before
    // the keychain is opened, so it never prompts for or loads a secret for
    // a run that cannot happen. `do`, `continue` and `watch` always confine
    // their agents (`AgentEnv::from_runner`); `OnDemand` checks again.
    system.sandbox().map_err(|error| error.to_string())?;
    // The machine's run slots every project's drain shares (OWL-409), in
    // Owlshift's data directory, which is the user's state directory the
    // design names. A step's child takes one before anything else, the
    // keychain included, and keeps it to its end; with none free, it writes
    // its "no slot" record and ends, its ticket untouched.
    let slots = MachineSlots::new(data_dir, concurrent_runs(&config.personal));
    let _slot = match &envelope {
        Some(envelope) => {
            let dirs = ProjectDirs::new(data_dir, &repo);
            let now = on_demand::system_clock();
            match step::take_slot(&slots, &dirs, &repo.to_string(), envelope, now)
                .map_err(|error| format!("the machine's run slots: {error}"))?
            {
                Some(slot) => Some(slot),
                None => return Ok(Ended::Code(ExitCode::SUCCESS)),
            }
        }
        None => None,
    };
    let Some(claude) = system.locate("claude") else {
        return Err(
            "`claude` is not on the PATH: install Claude Code (agent runs log in with the token `owlshift init` stores; `owlshift doctor` checks it)"
                .into(),
        );
    };
    let prompt = strip_role_front_matter(Role::Build, BUILD_ROLE)
        .map_err(|error| format!("the built-in build role: {error}"))?;
    let check_prompt = strip_role_front_matter(Role::AnswerCheck, ANSWER_CHECK_ROLE)
        .map_err(|error| format!("the built-in answer-check role: {error}"))?;
    let resolver_prompt = strip_role_front_matter(Role::Resolver, RESOLVER_ROLE)
        .map_err(|error| format!("the built-in resolver role: {error}"))?;
    let rebase_prompt = strip_role_front_matter(Role::Rebase, REBASE_ROLE)
        .map_err(|error| format!("the built-in rebase role: {error}"))?;
    let intake_prompt = strip_role_front_matter(Role::Intake, INTAKE_ROLE)
        .map_err(|error| format!("the built-in intake role: {error}"))?;
    let design_prompt = strip_role_front_matter(Role::Design, DESIGN_ROLE)
        .map_err(|error| format!("the built-in design role: {error}"))?;
    let design_review_prompt = strip_role_front_matter(Role::DesignReview, DESIGN_REVIEW_ROLE)
        .map_err(|error| format!("the built-in design review role: {error}"))?;
    let arbiter_prompt = strip_role_front_matter(Role::Arbiter, ARBITER_ROLE)
        .map_err(|error| format!("the built-in arbiter role: {error}"))?;
    let verify_prompt = strip_role_front_matter(Role::Verify, VERIFY_ROLE)
        .map_err(|error| format!("the built-in verify role: {error}"))?;
    // The personal file is valid here; were it not, no name would be allowed.
    // The names are those allowed for the repository this run delivers to.
    let allowed = config.allowed_gate_env(Some(&repo)).unwrap_or_default();
    // The caches declared, as far as the operator allows them for this
    // repository: the load refused any other (OWL-330).
    let allowed_caches = config.allowed_caches(Some(&repo)).unwrap_or_default();
    let caches: Vec<&str> = project
        .stack
        .cache_folders()
        .into_iter()
        .filter(|cache| allowed_caches.contains(cache))
        .collect();
    let agent = AgentEnv::from_runner(&project.stack.gate_env_names(), &allowed, &caches)
        .map_err(|error| format!("the agent environment: {error}"))?;

    // From here on, the credentials: the runner's own, never an agent's.
    let keychain = Arc::new(open_keychain().map_err(|error| error.to_string())?);
    // The project's own secrets, or the shared ones it has none of its own
    // for, each entry read once (OWL-308).
    let mut read = |account: &str| keychain.read(account).map_err(|error| error.to_string());
    let tracker_secrets = match project.tracker.kind {
        TrackerKind::Linear => Some(secrets::tracker(&repo, &mut read)?),
        TrackerKind::Markdown => None,
    };
    let forge_secret = secrets::forge(&repo, &mut read)?;
    // A step's child does not say again what its watch said at its start.
    if let Some(notice) = secrets::shared_notice(
        &repo,
        tracker_secrets.as_ref().map(|found| &found.accounts),
        &forge_secret.account,
    ) && envelope.is_none()
    {
        eprintln!("owlshift: {notice}");
    }
    // With the Linear app stored, its token is requested here, before any
    // work, and revoked when the tracker is dropped (OWL-157).
    let tracker: Box<dyn Tracker> = match &tracker_secrets {
        Some(found) => {
            let team = project
                .tracker
                .team
                .as_deref()
                .ok_or("a Linear project names its team in `tracker.team`")?;
            Box::new(tracker::linear(found, team)?)
        }
        None => Box::new(MarkdownTracker::new(root)),
    };
    if mode == Mode::Drain && !tracker.capabilities().contains(&Capability::ListAdmitted) {
        return Err("`owlshift watch --drain` needs a tracker that lists admitted tickets".into());
    }
    // The GitHub App's token is refreshed under the project's account lock
    // (OWL-384); one already expired is refreshed here, before any work.
    let dirs = ProjectDirs::new(data_dir, &repo);
    let access = forge::AppAccess {
        keychain: keychain.clone(),
        lock: dirs.account_lock_file(),
        app: forge::GitHubApp::from_personal(match &config.personal {
            FileState::Loaded { config, .. } => Some(config),
            _ => None,
        }),
    };
    let forge = forge::github(&forge_secret, repo.clone(), &access)?;
    // The token agent runs log in with (OWL-94), read with the other
    // secrets and set on the harness command alone. None stored refuses the
    // run, with the fix, before anything is cloned.
    let login = keychain
        .read(CLAUDE_AGENT_ACCOUNT)
        .map_err(|error| error.to_string())?;
    let budget = match &config.personal {
        FileState::Loaded { config, .. } => config
            .harnesses
            .claude
            .as_ref()
            .and_then(|claude| claude.budget_usd),
        _ => None,
    };
    // The standard tier without an effort, as section 9 routes intake and
    // the answer check; the resolver's router routes each of its runs.
    let harness = ClaudeHarness {
        program: claude,
        prompt,
        model: claude_model(&project.models, Tier::Standard).map(str::to_owned),
        effort: None,
        max_budget_usd: budget,
        login,
        bash: false,
    };
    // Build on the ticket's routing (OWL-305): the tier of each run picks
    // its model from `[models]`.
    let build = ClaudeRouter {
        harness: harness.clone(),
        models: project.models.clone(),
    };
    // The answer check runs on the same harness, model and login, with its
    // own prompt.
    let check = ClaudeHarness {
        prompt: check_prompt,
        ..harness.clone()
    };
    let resolver = resolver_router(&harness, resolver_prompt, &project.models);
    // Intake (OWL-248) on the standard tier.
    let intake = ClaudeHarness {
        prompt: intake_prompt,
        ..harness.clone()
    };
    // A rebase run (OWL-210) on the fast tier for a trivial conflict's first
    // run, `[models].fast` else the standard tier's model, then on the
    // ticket's tier.
    let rebase = ClaudeRouter {
        harness: ClaudeHarness {
            prompt: rebase_prompt,
            ..harness.clone()
        },
        models: project.models.clone(),
    };
    // Design on the ticket's routing, its reviewers on Claude Code on
    // another model than the author's (OWL-303, OWL-347), the arbiter on the
    // standard tier.
    let routed = |prompt: &str| ClaudeRouter {
        harness: ClaudeHarness {
            prompt: prompt.to_owned(),
            ..harness.clone()
        },
        models: project.models.clone(),
    };
    let design = routed(&design_prompt);
    let reviewer = routed(&design_review_prompt);
    let verifier = routed(&verify_prompt);
    let arbiter = routed(&arbiter_prompt);
    // The opt-in Codex reviewer (OWL-348), with each review stage's prompt,
    // when the project asks for it: whether Codex can serve is asked again
    // at each command's start, so a `codex login` made while `owlshift
    // watch` runs counts at its next command. A `codex` missing now is
    // missing for this process, its program being found here.
    let codex_program = system.locate("codex");
    let codex_login = user_login(&agent).unwrap_or_default();
    let codex_router = |prompt: &str| CodexRouter {
        harness: CodexHarness {
            program: codex_program
                .clone()
                .unwrap_or_else(|| PathBuf::from("codex")),
            prompt: prompt.to_owned(),
            model: None,
            effort: None,
            login: codex_login.clone(),
        },
        models: project.models.clone(),
    };
    let codex_design_review = codex_router(&design_review_prompt);
    let codex_verify = codex_router(&verify_prompt);
    let executor = on_demand::executor(git, agent);
    let codex_ready = || match &codex_program {
        Some(_) => codex_critic(system, &executor.agent).map(|_| ()),
        None => Err("`codex` was not on the PATH when this command started".to_owned()),
    };
    let codex_reviewer = CodexReviewer {
        design_review: &codex_design_review,
        verify: &codex_verify,
        ready: &codex_ready,
    };
    let codex = project.pipeline.codex_reviewer().then_some(codex_reviewer);
    // The review roles' fallback while Claude Code is at its usage limit
    // (OWL-406): Codex, on the same routers and readiness check, when the
    // personal file declares it; the fallback of a role other than a
    // reviewer is never used.
    let review_fallback = matches!(
        &config.personal,
        FileState::Loaded { config, .. }
            if config.harnesses.claude.as_ref().and_then(|claude| claude.fallback)
                == Some(Harness::Codex)
    )
    .then_some(codex_reviewer);
    let desktop = notify::desktop_enabled(&config.personal)
        .then(|| DesktopNotifier::for_this_machine(system))
        .flatten();
    let desktop = desktop.as_ref().map(|d| d as &dyn Notifier);
    // Every project's commands on this machine share each harness's usage
    // state (OWL-404).
    let usage = UsageStore::in_data_dir(data_dir)
        .with_caps(owlshift_runner::usage::caps_of(&config.personal));
    let on_demand = OnDemand {
        executor: &executor,
        tracker: tracker.as_ref(),
        forge: &forge,
        build: &build,
        intake: &intake,
        design: &design,
        design_review: &reviewer,
        verify: &verifier,
        codex,
        review_fallback,
        arbiter: &arbiter,
        answer_check: &check,
        resolver: &resolver,
        rebase: &rebase,
        remote_url: &remote_url,
        config: project,
        dirs: &dirs,
        head_wait: HEAD_WAIT,
        clock: &on_demand::system_clock,
        // The quarantine's notification, sent as its marker is written
        // (OWL-368), and a usage limit's, once per outage (OWL-404); the
        // others are `report`'s.
        notifier: desktop,
        usage: &usage,
        web_tools: owlshift_runner::config::web_tools_enabled(&config.personal),
    };
    let mut stdout = io::stdout();
    let mut sink = EventSink::new(repo.to_string(), EventLog::in_dir(data_dir), &mut stdout);
    let (ticket, outcome) = match (mode, &ticket) {
        (Mode::Do(_), Some(ticket)) => (ticket, on_demand.run(ticket, &mut sink)),
        (Mode::Continue(_), Some(ticket)) => (ticket, on_demand.continue_ticket(ticket, &mut sink)),
        (Mode::ToWatch(_), Some(ticket)) => {
            (ticket, on_demand.continue_to_watch(ticket, &mut sink))
        }
        (Mode::Step(..), Some(ticket)) => {
            let envelope = envelope.ok_or("a step without its envelope")?;
            // The machine kept from idle sleep while this child holds its
            // slot (OWL-439): the binding holds the assertion to the end of
            // the step, so it must stay named (`_awake`, never `_`), and it
            // is taken before the step's runs start, from this one thread.
            let _awake = step::keep_awake(
                owlshift_runner::config::keep_awake_enabled(&config.personal),
                ticket,
                &mut sink,
                || KeepAwake::hold(&format!("a step of {ticket}")),
            );
            // Reported as `continue`'s, then the record written last, which
            // its watch reads once this process has ended.
            let mut code = ExitCode::SUCCESS;
            let mut reported = |outcome: &_, sink: &mut EventSink<'_>| {
                code = report(system, desktop, tracker.as_ref(), ticket, outcome, sink);
            };
            step::child(&on_demand, &envelope, &mut sink, &mut reported).map_err(|error| {
                format!("the step's outcome record could not be written: {error}")
            })?;
            return Ok(Ended::Code(code));
        }
        _ => {
            // OWL-152: until a stop ends it (OWL-161); each continue's outcome
            // is reported as `continue`'s, and so is each intake and dispatch
            // of the drain (OWL-250).
            let draining = if mode == Mode::Drain {
                format!(
                    "; admitted tickets of {} are taken in, then built one at a time",
                    project.tracker.states.ready
                )
            } else {
                String::new()
            };
            let _ = writeln!(
                io::stdout(),
                "Watching {repo}: each ticket whose questions wait is continued once its \
                 decider's reply counts{draining}, checked every {} seconds. Ctrl-C ends it once \
                 the steps under way finish, a second stops them checking their runs, a third \
                 stops now.",
                POLL_INTERVAL.as_secs()
            );
            // Held while watch runs, which `continue --to-watch` reads
            // (OWL-420).
            let _presence = match presence::hold(&dirs) {
                Ok(Some(held)) => Some(held),
                Ok(None) => {
                    let _ = writeln!(
                        io::stderr(),
                        "owlshift: warning: another command kept this watch's mark, so \
                         `owlshift continue --to-watch` will say no watch runs"
                    );
                    None
                }
                Err(error) => {
                    let _ = writeln!(
                        io::stderr(),
                        "owlshift: warning: this watch's mark could not be kept ({error}), so \
                         `owlshift continue --to-watch` will say no watch runs"
                    );
                    None
                }
            };
            // Which build this watch runs, which `owlshift doctor` reads
            // (OWL-462).
            let watch_record = WatchRecord {
                format: Format,
                pid: std::process::id(),
                project: repo.to_string(),
                served: served.is_some(),
                since: Timestamp::now(),
                binary: std::env::current_exe()
                    .map(|binary| binary.display().to_string())
                    .unwrap_or_default(),
                version: OWLSHIFT_VERSION.to_owned(),
                build: own_build(),
            };
            let _recorded = match presence::record(&dirs, &watch_record) {
                Ok(recorded) => recorded,
                Err(error) => {
                    let _ = writeln!(
                        io::stderr(),
                        "owlshift: warning: this watch's record could not be kept ({error}), so \
                         `owlshift doctor` cannot tell which build it runs"
                    );
                    None
                }
            };
            let watch = Watch {
                on_demand: &on_demand,
                interval: POLL_INTERVAL,
            };
            let mut continued = |ticket: &TicketId, outcome: &_, sink: &mut EventSink<'_>| {
                report(system, desktop, tracker.as_ref(), ticket, outcome, sink);
            };
            // A served watch reads its default branch again between two
            // passes, and ends once a newer project file is accepted there,
            // its steps under way ending first, to start again on it
            // (OWL-434); a watch in a terminal only sleeps.
            let mut between = served.map(|_| Between::new(POLL_INTERVAL));
            let mut reload = false;
            let served_git = project::runner_git();
            let personal = owlshift_platform::paths::personal_config_file();
            let accept = |text: &str| served::accept(&repo, text, personal.clone());
            let mut served_out = io::stdout();
            let mut served_sink = EventSink::new(
                repo.to_string(),
                EventLog::in_dir(data_dir),
                &mut served_out,
            );
            let mut sleep = |interval| {
                // A stop asked by a signal wakes it (OWL-161): the loop's
                // own check then ends the watch, so it goes on here.
                if owlshift_platform::process::wait_asked(interval).is_some() {
                    return true;
                }
                if let (Some(served), Some(between)) = (served, between.as_mut())
                    && between.check(
                        &served_git,
                        served,
                        &accept,
                        desktop,
                        &mut served_sink,
                        &mut io::stdout(),
                        Instant::now(),
                    )
                {
                    reload = true;
                    return false;
                }
                true
            };
            if mode == Mode::Drain {
                // Each step in a child, this binary itself (OWL-399), on the
                // served project's kept file when served (OWL-434): the one
                // this process runs, where the system can tell, even once
                // replaced on disk (OWL-435).
                let program = owlshift_platform::process::own_program()
                    .map(|own| own.spawn)
                    .map_err(|error| {
                        format!("this program's own path, which runs each step: {error}")
                    })?;
                let served_repo = served.map(|served| served.repo.clone());
                // Each takes one of the machine's run slots (OWL-409).
                let children = Children::new(move |ticket, envelope| {
                    step_command(&program, ticket, envelope, served_repo.as_ref())
                })
                .with_slots(slots);
                watch.run_with_children(
                    true,
                    &children,
                    &mut sink,
                    &mut io::stdout(),
                    &mut continued,
                    &mut sleep,
                );
            } else {
                watch.run(&mut sink, &mut io::stdout(), &mut continued, &mut sleep);
            }
            return Ok(if reload {
                Ended::Reload
            } else {
                Ended::Code(ExitCode::SUCCESS)
            });
        }
    };
    Ok(Ended::Code(report(
        system,
        desktop,
        tracker.as_ref(),
        ticket,
        &outcome,
        &mut sink,
    )))
}

/// How many steps of `owlshift watch --drain` this machine runs at once
/// (OWL-409): the personal file's `concurrent_runs`, its default when the
/// file is absent. An invalid file never gets here: `loaded_project`
/// refuses it.
pub(crate) fn concurrent_runs(personal: &FileState<PersonalConfig>) -> u32 {
    match personal {
        FileState::Loaded { config, .. } => config.concurrent_runs(),
        _ => DEFAULT_CONCURRENT_RUNS,
    }
}

/// Shares the served lock of `repo`'s project in `data_dir` for a watch typed
/// in a terminal (OWL-435): refused while the background service serves the
/// project. A lock that cannot be read is a warning, the watch going on.
fn share_served_lock(data_dir: &Path, repo: &Repo) -> Result<Option<ServedLock>, Refusal> {
    match served::share_served(&ProjectDirs::new(data_dir, repo)) {
        Ok(Some(share)) => Ok(Some(share)),
        Ok(None) => Err(format!(
            "{repo} is served by the background service on this machine, which runs its watch: \
             `owlshift stop --forget` in its checkout stops serving it, `owlshift stop` stops the \
             service"
        )
        .into()),
        Err(error) => {
            let _ = writeln!(
                io::stderr(),
                "{}",
                printable(&format!(
                    "owlshift: warning: the served lock of {repo} could not be read ({error}), so \
                     this watch cannot tell whether the background service serves the project"
                ))
            );
            Ok(None)
        }
    }
}

/// The step `text` names for `ticket`: refused, before anything is read,
/// when it does not read or names another ticket.
fn envelope_of(text: &str, ticket: &TicketId) -> Result<StepEnvelope, String> {
    let envelope =
        StepEnvelope::parse(text).map_err(|error| format!("the step's envelope: {error}"))?;
    if envelope.ticket != *ticket {
        return Err(format!(
            "the step's envelope is {}'s, not {ticket}'s",
            envelope.ticket
        ));
    }
    Ok(envelope)
}

/// The child that runs a step of `ticket` for `owlshift watch --drain`:
/// `program`, this binary, as `owlshift continue <TICKET> --step
/// <ENVELOPE>` (OWL-399), with `--served <OWNER/REPO>` for a served
/// project's watch (OWL-434).
fn step_command(
    program: &Path,
    ticket: &TicketId,
    envelope: &str,
    served: Option<&Repo>,
) -> Command {
    let mut command = Command::new(program);
    command.args(["continue", ticket.as_str(), "--step", envelope]);
    if let Some(repo) = served {
        command.args(["--served", &repo.to_string()]);
    }
    command
}

/// The resolver's router (OWL-344): the shared harness with the resolver's
/// prompt and Bash, inside the read-only profile, on the tier and effort
/// `OnDemand::resolve` routes each run to.
fn resolver_router(harness: &ClaudeHarness, prompt: String, models: &Models) -> ClaudeRouter {
    ClaudeRouter {
        harness: ClaudeHarness {
            prompt,
            bash: true,
            ..harness.clone()
        },
        models: models.clone(),
    }
}

/// The project file `command` works with, and its path, from a valid
/// configuration: why it cannot otherwise.
pub(crate) fn loaded_project<'c>(
    config: &'c Effective,
    command: &str,
) -> Result<(&'c PathBuf, &'c ProjectConfig), String> {
    let loaded = match &config.project {
        FileState::Loaded { path, config, .. } => (path, config),
        FileState::Absent(path) => {
            return Err(format!(
                "no project file at {}: run `owlshift init` first",
                path.display()
            ));
        }
        FileState::NotApplicable(reason) => {
            return Err(format!("{command} runs in a git repository: {reason}"));
        }
        FileState::Unavailable(reason) => return Err(reason.clone()),
        FileState::Invalid { path, error } => {
            return Err(format!("{} is invalid: {error}", path.display()));
        }
    };
    if !config.is_valid() {
        return Err("the personal configuration is invalid: see `owlshift config show`".to_owned());
    }
    Ok(loaded)
}

/// The end of one run of a ticket, `do`'s, `continue`'s, or each continue
/// `watch` runs: its outcome printed ([`finish`]), then (OWL-140) a desktop
/// notification when it makes the operator the blocker, a failure of which
/// is a warning event.
fn report(
    system: &dyn System,
    desktop: Option<&dyn Notifier>,
    tracker: &dyn Tracker,
    ticket: &TicketId,
    outcome: &Result<Delivered, Stop>,
    sink: &mut EventSink<'_>,
) -> ExitCode {
    let code = finish(system, outcome, &mut io::stdout(), &mut io::stderr());
    notify::notify_blocker(desktop, tracker, ticket, outcome, sink);
    code
}

/// The end of a run: the warning of a sentinel that ended during it, or is
/// stopped at its end, then its outcome.
#[cfg_attr(not(unix), allow(unused_variables))]
fn finish(
    system: &dyn System,
    outcome: &Result<Delivered, Stop>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    // OWL-88: a sentinel that ended during the run left it unprotected, and
    // is never restarted; one that never started was warned of in `main`.
    // OWL-91, OWL-95: one stopped at the end still stops the live trees once
    // continued at Owlshift's end, with the exceptions the warning names;
    // one the guard killed during the run is reported as ended.
    #[cfg(unix)]
    {
        use owlshift_runner::system::SentinelStatus;
        match system.sentinel() {
            SentinelStatus::Running { .. } | SentinelStatus::NotRunning => {}
            SentinelStatus::Stopped { .. } => {
                let _ = writeln!(
                    stderr,
                    "owlshift: warning: its sentinel is stopped at the end of this run: the \
                     system continues it as Owlshift ends, and it then stops, best effort, the \
                     processes Owlshift started, unless it was stopped in its first milliseconds \
                     (before it ignores SIGHUP) or, on Linux, Owlshift's end leaves it to a \
                     subreaper in the same session"
                );
            }
            SentinelStatus::Ended(how) => {
                let _ = writeln!(
                    stderr,
                    "owlshift: warning: its sentinel ended during this run ({}): a hard kill of \
                     Owlshift would have left the processes it started running",
                    printable(&how)
                );
            }
        }
    }
    match outcome {
        Ok(delivered) => {
            let _ = writeln!(stdout, "\n{}", printable(&delivered.to_string()));
            ExitCode::SUCCESS
        }
        Err(stop) => {
            let _ = writeln!(stdout, "\n{}", printable(&stop.to_string()));
            // A look at a followed pull request is no failure (OWL-209),
            // nor is a ticket back in the ready column (OWL-248), nor a
            // stage step of watch's (OWL-367), nor a ticket left to watch
            // (OWL-420), nor a step a checked stop ended (OWL-161).
            if matches!(
                stop,
                Stop::Watched { .. }
                    | Stop::Ready { .. }
                    | Stop::Stepped { .. }
                    | Stop::ToWatch { .. }
                    | Stop::Stopped
            ) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};

    use owlshift_runner::system::{Captured, RunError, SentinelStatus};

    use super::*;

    /// A host whose sentinel is as said; a run's end asks it nothing else.
    struct Sentinel(SentinelStatus);

    impl System for Sentinel {
        fn locate(&self, _program: &str) -> Option<PathBuf> {
            unreachable!("the end of a run locates no program")
        }

        fn run(&self, _: &Path, _: &[&str], _: Option<&Path>) -> Result<Captured, RunError> {
            unreachable!("the end of a run runs no program")
        }

        fn sentinel(&self) -> SentinelStatus {
            self.0.clone()
        }
    }

    /// OWL-344: the resolver runs with Bash, on its own prompt, the shared
    /// harness's program, login and budget kept.
    #[test]
    fn the_resolver_is_built_with_bash() {
        let shared = ClaudeHarness {
            program: PathBuf::from("/opt/claude"),
            prompt: "# Build".to_owned(),
            model: Some("sonnet".to_owned()),
            effort: None,
            max_budget_usd: Some(3.0),
            login: None,
            bash: false,
        };
        let models = Models {
            deep: None,
            standard: None,
            fast: None,
        };
        let router = resolver_router(&shared, "# Resolver".to_owned(), &models);
        assert_eq!(
            router,
            ClaudeRouter {
                harness: ClaudeHarness {
                    prompt: "# Resolver".to_owned(),
                    bash: true,
                    ..shared.clone()
                },
                models,
            }
        );
    }

    /// What the end of a refused run writes, on stdout and on stderr.
    fn finish_refused(sentinel: SentinelStatus) -> (ExitCode, String, String) {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = finish(
            &Sentinel(sentinel),
            &Err(Stop::Refused("no ticket".to_owned())),
            &mut stdout,
            &mut stderr,
        );
        let text = |bytes| String::from_utf8(bytes).unwrap();
        (code, text(stdout), text(stderr))
    }

    /// OWL-420: a ticket left to watch is no failure, a watch running or
    /// not, and the line says when none drives it.
    #[test]
    fn a_ticket_left_to_watch_ends_in_success() {
        for (watching, said) in [(Some(false), true), (Some(true), false)] {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            let stop = Stop::ToWatch {
                ticket: TicketId::new("OWL-1").unwrap(),
                stage: owlshift_contracts::Stage::Build,
                takes: owlshift_runner::on_demand::Takes::Resume,
                restarted: true,
                watching,
            };
            let code = finish(
                &Sentinel(SentinelStatus::Running { pid: 1 }),
                &Err(stop),
                &mut stdout,
                &mut stderr,
            );
            assert_eq!(code, ExitCode::SUCCESS);
            let stdout = String::from_utf8(stdout).unwrap();
            assert!(
                stdout.starts_with(
                    "\nOWL-1 is restarted at Build, and nothing runs now: `owlshift watch` \
                     resumes it at its next pass"
                ),
                "{stdout}"
            );
            assert_eq!(
                stdout.contains("nothing drives the ticket"),
                said,
                "{stdout}"
            );
        }
    }

    #[test]
    fn a_sentinel_that_ended_during_the_run_is_warned_of() {
        let (code, stdout, stderr) =
            finish_refused(SentinelStatus::Ended("signal: 9 (SIGKILL)".to_owned()));
        assert_eq!(
            stderr,
            "owlshift: warning: its sentinel ended during this run (signal: 9 (SIGKILL)): a hard \
             kill of Owlshift would have left the processes it started running\n"
        );
        // The outcome still follows.
        assert_eq!(stdout, "\nNot run: no ticket\n");
        assert_eq!(code, ExitCode::FAILURE);

        let (_, _, stderr) = finish_refused(SentinelStatus::Ended("\u{1b}[2J".to_owned()));
        assert!(stderr.contains("(\\u{1b}[2J)"), "{stderr}");
    }

    #[test]
    fn a_sentinel_stopped_at_the_end_of_the_run_is_warned_of() {
        let (code, stdout, stderr) = finish_refused(SentinelStatus::Stopped { pid: 4242 });
        assert_eq!(
            stderr,
            "owlshift: warning: its sentinel is stopped at the end of this run: the system \
             continues it as Owlshift ends, and it then stops, best effort, the processes \
             Owlshift started, unless it was stopped in its first milliseconds (before it \
             ignores SIGHUP) or, on Linux, Owlshift's end leaves it to a subreaper in the same \
             session\n"
        );
        assert_eq!(stdout, "\nNot run: no ticket\n");
        assert_eq!(code, ExitCode::FAILURE);
    }

    #[test]
    fn a_sentinel_that_runs_or_never_started_is_not_warned_of() {
        for sentinel in [
            SentinelStatus::Running { pid: 4242 },
            // Warned of in `main`, when it failed to start.
            SentinelStatus::NotRunning,
        ] {
            let (code, stdout, stderr) = finish_refused(sentinel);
            assert_eq!(stderr, "");
            assert_eq!(stdout, "\nNot run: no ticket\n");
            assert_eq!(code, ExitCode::FAILURE);
        }
    }
}

#[cfg(test)]
mod refusals {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use owlshift_contracts::config::ProjectConfig;
    use owlshift_platform::sandbox::SandboxError;
    use owlshift_runner::system::{Captured, RunError};

    use super::*;

    /// A host where agent runs cannot be confined, asked nothing else.
    struct NoSandbox;

    impl System for NoSandbox {
        fn locate(&self, _program: &str) -> Option<PathBuf> {
            unreachable!("a host that cannot confine is refused before `claude` is looked for")
        }

        fn run(&self, _: &Path, _: &[&str], _: Option<&Path>) -> Result<Captured, RunError> {
            unreachable!("a host that cannot confine is refused before any program runs")
        }

        fn sandbox(&self) -> Result<(), SandboxError> {
            Err(SandboxError::Unsupported)
        }

        fn secret_stored(&self, _account: &str) -> Result<bool, String> {
            unreachable!("a host that cannot confine is refused before any secret is looked at")
        }
    }

    fn no_keychain() -> Result<Keychain, KeychainError> {
        panic!("the keychain was opened on a host that cannot confine agent runs")
    }

    #[test]
    fn a_host_that_cannot_confine_is_refused_before_the_keychain_is_opened() {
        // A project `do` would otherwise go on with: a Linear project of
        // team OWL, in a repository whose origin is on github.com.
        let repo = tempfile::tempdir().unwrap();
        for args in [
            &["init", "--quiet"][..],
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owlshift/demo.git",
            ],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
        let project = ProjectConfig::parse(
            "requires = \">=0.0\"\n[tracker]\nkind = \"linear\"\nteam = \"OWL\"\n\
             admit = \"delegation\"\n\
             states = { ready = \"a\", working = \"b\", needs_input = \"c\", review = \"d\" }\n\
             [stack]\ngate = []\n[pipeline]\ndefault = \"trivial\"\nplan_approval = \"never\"\n\
             [models]\n[policy]\nalways_human = []\n",
        )
        .unwrap();
        let config = Effective {
            project: FileState::Loaded {
                path: repo.path().join("owlshift.toml"),
                config: project,
                entries: Vec::new(),
            },
            personal: FileState::Absent(repo.path().join("config.toml")),
        };

        let data = tempfile::tempdir().unwrap();
        let step = envelope("OWL-1").render();
        for mode in [
            Mode::Do("OWL-1"),
            Mode::Continue("OWL-1"),
            Mode::ToWatch("OWL-1"),
            Mode::Watch,
            Mode::Step("OWL-1", &step),
        ] {
            let refusal = run_with(&NoSandbox, &config, mode, no_keychain, None, data.path())
                .err()
                .unwrap();
            assert_eq!(
                refusal.message,
                SandboxError::Unsupported.to_string(),
                "{mode:?}"
            );
            assert!(refusal.lasting, "{mode:?}");
        }

        // OWL-435: a watch typed in a terminal on a project the background
        // service serves is refused for that first, whatever the host.
        let served = data.path().join("projects/github.com/owlshift/demo");
        std::fs::create_dir_all(&served).unwrap();
        let lock = std::fs::File::create(served.join("served.lock")).unwrap();
        lock.try_lock().unwrap();
        let refusal = run_with(
            &NoSandbox,
            &config,
            Mode::Watch,
            no_keychain,
            None,
            data.path(),
        )
        .err()
        .unwrap();
        assert!(
            refusal
                .message
                .starts_with("owlshift/demo is served by the background service"),
            "{refusal}"
        );
        drop(lock);

        // OWL-399: a step whose envelope does not read, or is another
        // ticket's, is refused before anything else.
        let other = envelope("OWL-2").render();
        for (text, said) in [
            ("{", "the step's envelope: invalid step envelope"),
            (
                other.as_str(),
                "the step's envelope is OWL-2's, not OWL-1's",
            ),
        ] {
            let refusal = run_with(
                &NoSandbox,
                &config,
                Mode::Step("OWL-1", text),
                no_keychain,
                None,
                data.path(),
            )
            .err()
            .unwrap();
            assert!(refusal.message.starts_with(said), "{refusal}");
        }
    }

    /// OWL-435: a served watch that no restart mends ends with the code the
    /// daemon pauses its project on; a refusal a restart may mend, a watch
    /// typed in a terminal and any other command end in a plain failure.
    #[test]
    fn only_a_lasting_refusal_of_a_served_watch_pauses_its_project() {
        let lasting = Refusal::from("no Linear API key");
        let passing = Refusal::passing("Linear did not answer");
        assert_eq!(
            refused_code(&lasting, Mode::Drain, true),
            ExitCode::from(UNSERVABLE_EXIT)
        );
        for (refusal, mode, served) in [
            (&passing, Mode::Drain, true),
            (&lasting, Mode::Drain, false),
            (&lasting, Mode::Watch, false),
            (&lasting, Mode::Step("OWL-1", "{}"), true),
            (&lasting, Mode::Do("OWL-1"), false),
        ] {
            assert_eq!(
                refused_code(refusal, mode, served),
                ExitCode::FAILURE,
                "{mode:?}"
            );
        }
    }

    /// An intake's envelope for `ticket`.
    fn envelope(ticket: &str) -> StepEnvelope {
        StepEnvelope {
            format: owlshift_contracts::format::Format,
            ticket: TicketId::new(ticket).unwrap(),
            attempt: "1-2-3".to_owned(),
            step: owlshift_contracts::step::StepKind::Intake,
            commit: None,
            progress: "p".to_owned(),
            answer: None,
            generation: 0,
            kept: Default::default(),
        }
    }

    /// OWL-399: the drain's child is this binary, continuing the ticket with
    /// the step's envelope as one argument.
    #[test]
    fn the_drains_child_continues_the_ticket_with_its_envelope() {
        let text = envelope("OWL-1").render();
        let ticket = TicketId::new("OWL-1").unwrap();
        let command = step_command(Path::new("/opt/owlshift"), &ticket, &text, None);
        assert_eq!(command.get_program(), "/opt/owlshift");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["continue", "OWL-1", "--step", text.as_str()]);
        // A served project's step runs on its watch's kept file (OWL-434).
        let repo = Repo::parse("acme/web").unwrap();
        let command = step_command(Path::new("/opt/owlshift"), &ticket, &text, Some(&repo));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "continue",
                "OWL-1",
                "--step",
                text.as_str(),
                "--served",
                "acme/web"
            ]
        );
    }
}
