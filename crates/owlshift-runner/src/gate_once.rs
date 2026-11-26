//! `owlshift gate` (OWL-329): the project's gate, run once, alone, as a
//! Build's gate runs it, so an operator can try a gate in seconds rather
//! than in a Build run and a park.
//!
//! [`GateOnce::run`] takes what a Build's preparation takes, in its order:
//! git's minimum version, the project's lock, the quarantine (OWL-400), the
//! confinement; then removes what an earlier `owlshift gate` left, brings the
//! dedicated checkout up to date ([`project::sync_checkout`]) and picks the
//! commit, origin's default branch or the one `--at` names, read as a run's
//! base. The gate is the `stack.gate` of the project file at that commit, or
//! the operator's own when the commit has none. [`Executor::run_gate`] runs
//! it in a fresh worktree on [`GATE_BRANCH`], through the code a Build's gate
//! runs through: the same agent environment, sandbox, temporary folder,
//! credential check and isolation checks. A breach marks the project as a
//! run's would, and leaves the worktree for the person who looks; otherwise
//! the worktree and its branch are removed. No harness, tracker or forge.
//!
//! The same run checks a Build's base (OWL-418, [`Place::Base`]): from the
//! step of the ticket whose gate or setup failed, at the commit its run was
//! configured at, in a worktree and on a branch of its own. Either place
//! keeps its verdict in the project's base checks ([`crate::base`]) under the
//! project's lock: a base check always, `owlshift gate` only on a commit
//! whose check holds or is due, so a pass lifts a hold and the operator's
//! gate never opens one. A base check finds its verdict kept at the default
//! branch's head already, read under the lock, and runs nothing.
//!
//! `owlshift gate` runs alone, under the project's lock for its whole
//! length. A base check runs beside the project's other steps (OWL-660;
//! architecture section 5, "A broken base", "The check, beside the steps"):
//! the project's lock for short sections only, the base lock for its length,
//! one base check of the project at a time; its record (`base/run.json`,
//! [`ticket_run::write_base_run`]) tells siblings' isolation checks what to
//! leave out, its own check is told of every other ticket, and its
//! obligation names its ticket, whose lock its step holds.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use jiff::Timestamp;

use crate::base;
use crate::executor::gate::GATE_LOG;
use crate::executor::{Executor, GateRun, GateSpec};
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::ids::TicketId;
use owlshift_core::base::{After, Failing, Recorded, Verdict};

use crate::project::{self, Base, BaseLock, ProjectDirs, ProjectLock};
use crate::ticket_run::{self, Look, Siblings};

/// The branch of `owlshift gate`'s worktree: no ticket's branch
/// (`owlshift/<id>`) can take it.
pub const GATE_BRANCH: &str = "owlshift-gate/check";

/// The branch of a base check's worktree (OWL-418).
pub const BASE_BRANCH: &str = "owlshift-gate/base";

/// Where a run of the gate works, beside what, and what it keeps of its
/// verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// `owlshift gate`: alone, refused while any ticket's step is in
    /// flight; its verdict replaces a base check's that holds or is due, and
    /// is kept nowhere else.
    Gate,
    /// A Build's base check (OWL-418): beside the project's other steps
    /// (OWL-660); its verdict is always kept.
    Base,
}

impl Place {
    fn folder(self) -> &'static str {
        match self {
            Self::Gate => "gate",
            Self::Base => "base",
        }
    }

    fn branch(self) -> &'static str {
        match self {
            Self::Gate => GATE_BRANCH,
            Self::Base => BASE_BRANCH,
        }
    }

    /// How its obligation and a breach name the run.
    fn what(self) -> &'static str {
        match self {
            Self::Gate => "`owlshift gate`",
            Self::Base => "The base check",
        }
    }
}

/// How many times a base check waits the project lock's bound for its last
/// section (OWL-660): its verdict, after a gate of half an hour, is worth
/// minutes more than one holder's moment.
const LAST_SECTION_TRIES: usize = 5;

/// How a base check that held the base lock ended: its gate's run, the
/// quarantine a breach wrote, warnings, and what became of its verdict.
type Beside = (
    Option<GateRun>,
    Option<PathBuf>,
    Vec<String>,
    Option<BaseKept>,
);

/// The most lines of each command's output [`render`] prints; the gate log
/// has them all.
pub const TAIL_LINES: usize = 20;

/// One `owlshift gate`.
#[derive(Clone, Copy, Debug)]
pub struct GateOnce<'a> {
    /// The executor a Build gets: its git, its agent environment, its forge
    /// hosts and gate deadline.
    pub executor: &'a Executor,
    pub dirs: &'a ProjectDirs,
    /// The URL of `origin`, which the dedicated checkout clones.
    pub remote_url: &'a str,
    /// The operator's project file, the one the command loaded: its gate
    /// runs when the commit has no project file, and its `stack.gate_env`
    /// made the agent environment, as for a Build (OWL-331).
    pub local: &'a ProjectConfig,
    /// The commit to check, a full id or a remote-tracking name such as
    /// `origin/fix-gate`; origin's default branch when `None`.
    pub at: Option<&'a str>,
    /// Where it works (OWL-418).
    pub place: Place,
    /// The ticket whose step runs a base check, which holds its own lock:
    /// its obligation names it, and its own check compares that ticket's
    /// refs whole. `None` for `owlshift gate`.
    pub except: Option<&'a TicketId>,
    /// The failure a base check runs for: matched with its verdict.
    pub initiating: Option<&'a Failing>,
    /// The time its verdict is kept at.
    pub now: Timestamp,
}

/// Where the gate commands come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The project file at the commit checked.
    Commit,
    /// The operator's own project file: the commit has none.
    Local,
}

/// What is about to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The commit checked, a full id.
    pub commit: String,
    /// The name it was given: `origin/<default branch>`, or `--at`'s.
    pub name: String,
    pub source: Source,
    /// The default branch's head the checkout was brought to.
    pub head: String,
    pub commands: Vec<String>,
    /// The project's setup, run before the commands (OWL-330), from the same
    /// project file.
    pub setup: Vec<String>,
    /// The gate log.
    pub log: PathBuf,
    /// The names the commit's `stack.gate_env` declares, when they differ
    /// from those the agent environment passes, the operator's: a Build
    /// warns of it too (`gate_env_differs`).
    pub gate_env_differs: Option<Vec<String>>,
    /// The folders the commit's `stack.caches` declares, when they differ
    /// from those the agent environment opens, the operator's file's: a
    /// Build warns of it too (`caches_differ`, OWL-330).
    pub caches_differ: Option<Vec<String>>,
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match self.source {
            Source::Commit => "its project file",
            Source::Local => "your owlshift.toml, the commit having none",
        };
        writeln!(
            f,
            "The gate of {} ({}), from {source}, in a fresh worktree, confined as a Build's gate:",
            short(&self.commit),
            self.name
        )?;
        if !self.setup.is_empty() {
            writeln!(f, "  after its setup:")?;
            for command in &self.setup {
                writeln!(f, "    {command}")?;
            }
        }
        for command in &self.commands {
            writeln!(f, "  {command}")?;
        }
        if let Some(declared) = &self.gate_env_differs {
            writeln!(
                f,
                "Note: the commit's stack.gate_env declares [{}]; the gate gets the variables \
                 your owlshift.toml declares, as a Build's does.",
                declared.join(", ")
            )?;
        }
        if let Some(declared) = &self.caches_differ {
            writeln!(
                f,
                "Note: the commit's stack.caches declares [{}]; the gate reads the caches \
                 your owlshift.toml declares, as a Build's does.",
                declared.join(", ")
            )?;
        }
        writeln!(f, "Its whole output goes to {}.", self.log.display())
    }
}

/// `declared`, sorted, when it differs from `passed` once both are sorted.
fn differs(mut declared: Vec<&str>, mut passed: Vec<&str>) -> Option<Vec<String>> {
    declared.sort_unstable();
    passed.sort_unstable();
    (declared != passed).then(|| declared.iter().map(|n| (*n).to_owned()).collect())
}

/// What `owlshift gate` came to.
#[derive(Debug)]
pub struct Checked {
    pub plan: Plan,
    /// The gate's run; `None` when it has no command.
    pub run: Option<GateRun>,
    /// The time from the start of the command.
    pub elapsed: Duration,
    /// The marker a breach was recorded in; `None` without a breach.
    pub marker: Option<PathBuf>,
    /// What could not be cleaned up or recorded after the gate.
    pub warnings: Vec<String>,
    /// What became of the project's base check of the commit (OWL-418):
    /// `None` when nothing was kept.
    pub base: Option<BaseKept>,
}

/// A verdict kept in the project's base checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseKept {
    /// The verdict: the run's, or the one kept at this head already, in
    /// which case nothing ran; `None` when the run said nothing of the
    /// commands, a runner's fault or a failure around them.
    pub verdict: Option<Verdict>,
    /// How it changed the hold; `None` for a verdict not kept, or one kept
    /// already that no failure was matched with.
    pub recorded: Option<Recorded>,
    /// Whether the verdict was found kept at this head, nothing run.
    pub reused: bool,
    /// The key of the commands checked.
    pub commands: String,
    /// Why the verdict could not be read or kept, when it could not.
    pub unkept: Option<String>,
}

impl Checked {
    /// Whether the gate passed, isolation held and everything after it was
    /// cleaned up: `owlshift gate` exits 0 then.
    pub fn passed(&self) -> bool {
        self.warnings.is_empty() && self.run.as_ref().is_none_or(GateRun::passed)
    }
}

/// Why `owlshift gate` did not run the gate.
#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// Another command works the project.
    Busy(PathBuf),
    /// A run, or an `owlshift gate`, ended before its isolation check
    /// passed.
    Unverified {
        marker: PathBuf,
        text: String,
    },
    Refused(String),
    /// A checked stop of the step running a base check (OWL-161): the check
    /// did not start, or failed while the stop was asked, which may be the
    /// stop's doing; no verdict was kept.
    Stopped,
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy(root) => write!(
                f,
                "another owlshift command works this project ({}): try again once it ends",
                root.display()
            ),
            Self::Unverified { marker, text } => write!(
                f,
                "the project is refused until a person looks: {}\nOnce done, run `owlshift \
                 unquarantine` to clear {}.",
                text.trim_end(),
                marker.display()
            ),
            Self::Refused(reason) => f.write_str(reason),
            Self::Stopped => f.write_str("the check was stopped by a checked stop"),
        }
    }
}

/// The worktree of `owlshift gate` under the project's folder.
pub fn worktree(dirs: &ProjectDirs) -> PathBuf {
    worktree_at(dirs, Place::Gate)
}

/// The folder of its log, hidden from the gate.
pub fn run_dir(dirs: &ProjectDirs) -> PathBuf {
    run_dir_at(dirs, Place::Gate)
}

/// The worktree of a run of the gate alone at `place`.
pub fn worktree_at(dirs: &ProjectDirs, place: Place) -> PathBuf {
    dirs.root().join(place.folder()).join("worktree")
}

/// The folder of its log.
pub fn run_dir_at(dirs: &ProjectDirs, place: Place) -> PathBuf {
    dirs.root().join(place.folder()).join("run")
}

impl<'a> GateOnce<'a> {
    /// `owlshift gate` as the operator types it: at [`Place::Gate`], for no
    /// ticket and no failure, its verdict kept at the system clock's time.
    pub fn operator(
        executor: &'a Executor,
        dirs: &'a ProjectDirs,
        remote_url: &'a str,
        local: &'a ProjectConfig,
        at: Option<&'a str>,
    ) -> Self {
        Self {
            executor,
            dirs,
            remote_url,
            local,
            at,
            place: Place::Gate,
            except: None,
            initiating: None,
            now: Timestamp::now(),
        }
    }
}

impl GateOnce<'_> {
    /// Runs the gate; see the module documentation. `starting` is told what
    /// runs just before it does.
    pub fn run(&self, starting: &mut dyn FnMut(&Plan)) -> Result<Checked, Stop> {
        match self.place {
            Place::Gate => self.run_alone(starting),
            Place::Base => self.run_beside(starting),
        }
    }

    /// `owlshift gate`: alone, under the project's lock for its whole length.
    fn run_alone(&self, starting: &mut dyn FnMut(&Plan)) -> Result<Checked, Stop> {
        let started = Instant::now();
        let executor = self.executor;
        let dirs = self.dirs;
        // As a Build's preparation: the version, the lock, then the marker,
        // then the confinement (`on_demand`'s `prepare`).
        executor
            .git
            .require_minimum_version()
            .map_err(Stop::Refused)?;
        let _lock = self.section()?;
        self.look()?;
        // Alone (OWL-401): a step in flight holds its ticket's lock and not
        // the project's, and this gate's check is a run's alone, so it waits
        // for none to stand; no step starts meanwhile, every one starting
        // with the project's lock this gate holds.
        match ticket_run::ticket_in_flight(dirs) {
            Ok(None) => {}
            Ok(Some(_)) => return Err(Stop::Busy(dirs.root().to_owned())),
            Err(error) => {
                return Err(Stop::Refused(format!("the project's tickets: {error}")));
            }
        }
        executor
            .agent
            .sandbox_ready()
            .map_err(|e| Stop::Refused(e.to_string()))?;
        let checkout = dirs.checkout();
        let worktree = worktree_at(dirs, self.place);
        let branch = self.place.branch();
        // What a stopped `owlshift gate` left, once its marker is gone.
        if fs::symlink_metadata(&checkout).is_ok() {
            executor
                .discard_worktree(&checkout, &worktree, branch)
                .map_err(|e| {
                    Stop::Refused(format!(
                        "removing what an earlier `owlshift gate` left: {e}"
                    ))
                })?;
        }
        let plan = self.plan()?;
        let checked = |run, marker, warnings, base| Checked {
            plan: plan.clone(),
            run,
            elapsed: started.elapsed(),
            marker,
            warnings,
            base,
        };
        if plan.commands.is_empty() {
            return Ok(checked(None, None, Vec::new(), None));
        }
        let key = base::commands_key(&plan.setup, &plan.commands);
        // A checked stop of the step (OWL-161) runs no check, as it starts
        // no run.
        if crate::executor::checked_stop_asked() {
            return Err(Stop::Stopped);
        }
        let obligation = self.obligation(&plan, None, &worktree)?;
        starting(&plan);
        let run_dir = run_dir_at(dirs, self.place);
        let spec = self.spec(&plan, (&checkout, &worktree, &run_dir));
        let discard = || executor.discard_worktree(&checkout, &worktree, branch);
        let run = match executor.run_gate(&spec) {
            Ok(run) => run,
            Err(error) => {
                // No gate command ran: nothing to verify, as for a run.
                let mut reason = format!("the gate could not start: {error}");
                if let Err(e) = discard() {
                    reason.push_str(&format!("; its worktree could not be removed: {e}"));
                }
                if let Err(e) = ticket_run::remove_obligation(dirs, &obligation) {
                    reason.push_str(&format!("; the gate's obligation: {e}"));
                }
                return Err(Stop::Refused(reason));
            }
        };
        let mut warnings = Vec::new();
        if !run.violations.is_empty() {
            let marker = Some(self.breached(&plan, &run, &worktree, &mut warnings));
            return Ok(checked(Some(run), marker, warnings, None));
        }
        if let Err(e) = discard() {
            warnings.push(format!("the gate's worktree could not be removed: {e}"));
        }
        if let Err(e) = ticket_run::remove_obligation(dirs, &obligation) {
            warnings.push(format!("the gate's obligation could not be removed: {e}"));
        }
        // A failure while a checked stop is asked may be the stop's, which
        // cuts the gate's commands (OWL-161): no verdict is kept.
        if !run.report.passed() && crate::executor::checked_stop_asked() {
            return Err(Stop::Stopped);
        }
        // Kept before the project's lock goes (OWL-418).
        let kept = self.keep(&plan, &key, &run);
        Ok(checked(Some(run), None, warnings, kept))
    }

    /// A step's base check (OWL-418), beside the project's other steps
    /// (OWL-660; architecture section 5, "A broken base", "The check, beside
    /// the steps"): the project's lock for sections only, the base lock from
    /// the moment the check is known to run until its end, and its record
    /// written whatever the end, before that lock goes.
    fn run_beside(&self, starting: &mut dyn FnMut(&Plan)) -> Result<Checked, Stop> {
        let started = Instant::now();
        let executor = self.executor;
        let dirs = self.dirs;
        executor
            .git
            .require_minimum_version()
            .map_err(Stop::Refused)?;
        let Some(ticket) = self.except else {
            return Err(Stop::Refused(
                "a base check runs from a ticket's step".to_owned(),
            ));
        };
        // The first section: the quarantine, the fetch, the commit and its
        // project file, and a verdict kept at this head, used with no base
        // lock taken, so a Build on a commit already checked never waits for
        // another check.
        let section = self.section()?;
        self.look()?;
        executor
            .agent
            .sandbox_ready()
            .map_err(|e| Stop::Refused(e.to_string()))?;
        let plan = self.plan()?;
        let checked = |run, marker, warnings, base| Checked {
            plan: plan.clone(),
            run,
            elapsed: started.elapsed(),
            marker,
            warnings,
            base,
        };
        if plan.commands.is_empty() {
            return Ok(checked(None, None, Vec::new(), None));
        }
        let key = base::commands_key(&plan.setup, &plan.commands);
        // A base check another ticket's step made at this head is used,
        // under the lock, nothing run (OWL-418).
        if let Some(kept) = self.reuse(&plan, &key) {
            return Ok(checked(None, None, Vec::new(), Some(kept)));
        }
        drop(section);
        // One base check of the project at a time: a second waits for the
        // first (OWL-660).
        let lock = match dirs.base_lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => return Err(Stop::Busy(dirs.base_dir())),
            Err(error) => return Err(Stop::Refused(format!("the base lock: {error}"))),
        };
        let mut record =
            ticket_run::read_base_run(dirs).map_or((None, None), |run| (run.tip, run.entry));
        let ended = self.beside_locked(&lock, ticket, (&plan, &key), &mut record, starting);
        // Whatever the end, the record says where the base branch was left
        // before the base lock goes, so a sibling's check holds it to that
        // or to none.
        let checkout = dirs.checkout();
        if let Ok(tip) = ticket_run::branch_tip(&executor.git, &checkout, BASE_BRANCH) {
            record.0 = tip;
        }
        let unrecorded = ticket_run::write_base_run(dirs, &lock, record.0, record.1)
            .err()
            .map(|e| format!("the base check's record could not be written: {e}"));
        drop(lock);
        ended.map(|(run, marker, mut warnings, kept)| {
            warnings.extend(unrecorded);
            checked(run, marker, warnings, kept)
        })
    }

    /// The part of [`Self::run_beside`] that holds the base lock: what an
    /// earlier check left removed, the worktree made, the gate run outside
    /// the project's lock, told of the other tickets' steps, then the verdict
    /// kept in a last section. `record`, the base branch's commit and the
    /// worktree's entry, is kept up to date for the caller's last write.
    fn beside_locked(
        &self,
        lock: &BaseLock,
        ticket: &TicketId,
        (plan, key): (&Plan, &str),
        record: &mut (Option<String>, Option<String>),
        starting: &mut dyn FnMut(&Plan),
    ) -> Result<Beside, Stop> {
        let executor = self.executor;
        let dirs = self.dirs;
        let checkout = dirs.checkout();
        let worktree = worktree_at(dirs, self.place);
        let discard = || executor.discard_own_worktree(&checkout, &worktree, BASE_BRANCH);
        let section = self.section()?;
        // A sibling's breach may have written the quarantine since.
        self.look()?;
        // A checked stop of the step (OWL-161) runs no check, as it starts
        // no run.
        if crate::executor::checked_stop_asked() {
            return Err(Stop::Stopped);
        }
        // What a stopped check left: its entries found by their `gitdir`,
        // never by `git worktree prune`, which would remove entries a
        // sibling's check reads.
        discard()
            .map_err(|e| Stop::Refused(format!("removing what an earlier base check left: {e}")))?;
        let obligation = self.obligation(plan, Some(ticket), &worktree)?;
        starting(plan);
        let run_dir = run_dir_at(dirs, self.place);
        let spec = self.spec(plan, (&checkout, &worktree, &run_dir));
        let made = executor
            .make_gate_worktree(&spec)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                *record = (
                    Some(plan.commit.clone()),
                    ticket_run::entry_of(&checkout, &worktree),
                );
                ticket_run::write_base_run(dirs, lock, record.0.clone(), record.1.clone())
                    .map_err(|e| format!("its record could not be written: {e}"))
            });
        if let Err(error) = made {
            let mut reason = format!("the gate could not start: {error}");
            if let Err(e) = discard() {
                reason.push_str(&format!("; its worktree could not be removed: {e}"));
            }
            if let Err(e) = ticket_run::remove_obligation(dirs, &obligation) {
                reason.push_str(&format!("; the gate's obligation: {e}"));
            }
            return Err(Stop::Refused(reason));
        }
        drop(section);
        // The gate and its isolation check, outside the project's lock: told
        // of every other ticket's step, its own ticket's refs compared whole.
        let siblings = Siblings::for_base_check(dirs, ticket);
        let run = match executor.gate_in(&spec, Some(&siblings)) {
            Ok(run) => run,
            Err(error) => {
                let mut reason = format!("the gate could not start: {error}");
                match self.last_section() {
                    Some(_section) => {
                        if let Err(e) = discard() {
                            reason.push_str(&format!("; its worktree could not be removed: {e}"));
                        }
                    }
                    None => reason.push_str(
                        "; its worktree is left for the next base check, the project's lock \
                         staying held",
                    ),
                }
                if let Err(e) = ticket_run::remove_obligation(dirs, &obligation) {
                    reason.push_str(&format!("; the gate's obligation: {e}"));
                }
                return Err(Stop::Refused(reason));
            }
        };
        let mut warnings = Vec::new();
        if !run.violations.is_empty() {
            let marker = Some(self.breached(plan, &run, &worktree, &mut warnings));
            return Ok((Some(run), marker, warnings, None));
        }
        // The check passed: its obligation goes at once, so no wait below
        // can leave it behind.
        if let Err(e) = ticket_run::remove_obligation(dirs, &obligation) {
            warnings.push(format!("the gate's obligation could not be removed: {e}"));
        }
        let unkept = |why: String| {
            Some(BaseKept {
                verdict: base::verdict(&run.report).ok(),
                recorded: None,
                reused: false,
                commands: key.to_owned(),
                unkept: Some(why),
            })
        };
        let Some(section) = self.last_section() else {
            warnings.push(
                "the base check's worktree is left for the next one, the project's lock staying \
                 held"
                    .to_owned(),
            );
            if !run.report.passed() && crate::executor::checked_stop_asked() {
                return Err(Stop::Stopped);
            }
            let kept = unkept("the project's lock stayed held once the check ended".to_owned());
            return Ok((Some(run), None, warnings, kept));
        };
        // Before the branch and the entry go, the record says the branch's
        // commit, so a sibling whose check spans their removal finds it
        // there or gone; a record not written leaves them in place.
        if let Ok(tip) = ticket_run::branch_tip(&executor.git, &checkout, BASE_BRANCH) {
            record.0 = tip;
        }
        match ticket_run::write_base_run(dirs, lock, record.0.clone(), record.1.clone()) {
            Ok(()) => {
                if let Err(e) = discard() {
                    warnings.push(format!("the gate's worktree could not be removed: {e}"));
                }
            }
            Err(e) => warnings.push(format!(
                "the base check's worktree is left for the next one, its record not written: {e}"
            )),
        }
        // A failure while a checked stop is asked may be the stop's, which
        // cuts the gate's commands (OWL-161): no verdict is kept.
        if !run.report.passed() && crate::executor::checked_stop_asked() {
            return Err(Stop::Stopped);
        }
        // A quarantine written while the check ran: the checkout may hold
        // what a breaching run changed, so the verdict is not kept.
        let kept = match dirs.quarantine_at() {
            Ok(None) => self.keep(plan, key, &run),
            Ok(Some(_)) => unkept("the project was quarantined while the check ran".to_owned()),
            Err(e) => unkept(format!("the project's quarantine: {e}")),
        };
        drop(section);
        Ok((Some(run), None, warnings, kept))
    }

    /// The project's lock for one of a check's sections: busy once its bound
    /// is spent.
    fn section(&self) -> Result<ProjectLock, Stop> {
        match self.dirs.lock() {
            Ok(Some(lock)) => Ok(lock),
            Ok(None) => Err(Stop::Busy(self.dirs.root().to_owned())),
            Err(error) => Err(Stop::Refused(format!("the project's lock: {error}"))),
        }
    }

    /// A base check's last section, once a gate that may have run for half
    /// an hour ended: waited for up to [`LAST_SECTION_TRIES`] times the
    /// lock's bound rather than lose its verdict to one holder's moment.
    fn last_section(&self) -> Option<ProjectLock> {
        (0..LAST_SECTION_TRIES).find_map(|_| self.dirs.lock().ok().flatten())
    }

    /// The quarantine, or a run left behind, which becomes it (OWL-400).
    fn look(&self) -> Result<(), Stop> {
        match ticket_run::look(self.dirs, self.except) {
            Ok(Look::Clear { .. }) => Ok(()),
            Ok(Look::Quarantined { path, text }) => Err(Stop::Unverified { marker: path, text }),
            Err(error) => Err(Stop::Refused(format!("the project's quarantine: {error}"))),
        }
    }

    /// Brings the dedicated checkout up to date and says what runs: the
    /// commit, the gate and setup of its project file, and the default
    /// branch's head. The caller holds the project's lock.
    fn plan(&self) -> Result<Plan, Stop> {
        let executor = self.executor;
        let git = &executor.git;
        let dirs = self.dirs;
        let checkout = dirs.checkout();
        let base = project::sync_checkout(git, dirs, self.remote_url).map_err(Stop::Refused)?;
        let head = base.commit.clone();
        let (commit, name) = match self.at {
            Some(at) => (
                executor
                    .resolve_commit(&checkout, at)
                    .map_err(|e| Stop::Refused(format!("--at {e}")))?,
                at.to_owned(),
            ),
            None => (base.commit.clone(), base.remote_ref.clone()),
        };
        let at_commit = Base {
            remote_ref: name.clone(),
            branch: base.branch,
            commit: commit.clone(),
        };
        // As a Build takes its gate (OWL-331): the commit's project file
        // wins, the operator's only when the commit has none.
        let found = project::project_file_at(git, &checkout, &at_commit).map_err(Stop::Refused)?;
        let (config, source) = match &found {
            Some(config) => (config, Source::Commit),
            None => (self.local, Source::Local),
        };
        let mut declared = config.stack.gate_env_names();
        let mut passed = self.local.stack.gate_env_names();
        declared.sort_unstable();
        passed.sort_unstable();
        let gate_env_differs =
            (declared != passed).then(|| declared.iter().map(|n| (*n).to_owned()).collect());
        let caches_differ = differs(
            config.stack.cache_folders(),
            self.local.stack.cache_folders(),
        );
        Ok(Plan {
            commit,
            name,
            source,
            head,
            commands: config.stack.gate.clone(),
            setup: config.stack.setup.clone(),
            log: run_dir_at(dirs, self.place).join(GATE_LOG),
            gate_env_differs,
            caches_differ,
        })
    }

    /// Writes the obligation of the run about to start (OWL-400): naming
    /// `ticket`, the one whose step runs a base check and holds its lock
    /// (OWL-660), none for `owlshift gate`. From here until the isolation
    /// check passes, a stop leaves it, which refuses the project.
    fn obligation(
        &self,
        plan: &Plan,
        ticket: Option<&TicketId>,
        worktree: &Path,
    ) -> Result<String, Stop> {
        let stamp = Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string();
        // A base check's run id is never one of its ticket's runs'.
        let run = match ticket {
            Some(_) => format!("{stamp}-base"),
            None => stamp,
        };
        ticket_run::write_obligation(
            self.dirs,
            ticket,
            &run,
            &format!(
                "{} on {}, its worktree at {}",
                self.place.what(),
                plan.commit,
                worktree.display()
            ),
        )
        .map_err(|e| Stop::Refused(format!("the gate's obligation: {e}")))
    }

    /// The run of the gate at this place, in `worktree` of `checkout`, its
    /// log in `run_dir`.
    fn spec<'s>(
        &self,
        plan: &'s Plan,
        (checkout, worktree, run_dir): (&'s Path, &'s Path, &'s Path),
    ) -> GateSpec<'s> {
        GateSpec {
            main: checkout,
            worktree,
            branch: self.place.branch(),
            base: &plan.commit,
            run_dir,
            commands: &plan.commands,
            setup: &plan.setup,
        }
    }

    /// A breach of the gate's run: the worktree stays for the person who
    /// looks, its obligation stays until `owlshift unquarantine` (OWL-400),
    /// and the quarantine is written, its time making the text unique, as a
    /// run's is (OWL-368). Returns the quarantine's file, or `None` with a
    /// warning when it could not be written.
    fn breached(
        &self,
        plan: &Plan,
        run: &GateRun,
        worktree: &Path,
        warnings: &mut Vec<String>,
    ) -> PathBuf {
        let lines: Vec<String> = run.violations.iter().map(|v| format!("- {v}")).collect();
        let text = format!(
            "{} on {} broke isolation at {}; its worktree is kept at {}:\n{}\n",
            self.place.what(),
            plan.commit,
            Timestamp::now(),
            worktree.display(),
            lines.join("\n")
        );
        if let Err(e) = self.dirs.write_quarantine(&text) {
            warnings.push(format!("the breach could not be recorded: {e}"));
        }
        self.dirs.quarantine_file()
    }

    /// The verdict of a base check kept at `plan`'s head already, matched
    /// with the failure the check runs for, the match kept: `None` when none
    /// is kept at this head, or the file does not read, so the check runs.
    fn reuse(&self, plan: &Plan, key: &str) -> Option<BaseKept> {
        let mut checks = base::read(self.dirs).ok()?;
        let verdict = checks.verdict_at(&plan.commit, key, &plan.head)?.clone();
        let mut kept = BaseKept {
            verdict: Some(verdict),
            recorded: None,
            reused: true,
            commands: key.to_owned(),
            unkept: None,
        };
        if let Some(failing) = self.initiating
            && let After::Matched(recorded) = checks.after_failure(
                &plan.commit,
                key,
                &plan.head,
                failing,
                self.now.as_millisecond(),
            )
        {
            kept.recorded = Some(recorded);
            kept.unkept = base::write(self.dirs, &checks).err();
        }
        Some(kept)
    }

    /// Keeps `run`'s verdict in the project's base checks: a base check's
    /// always, matched with the failure it ran for; `owlshift gate`'s only
    /// on a commit whose check holds or is due. `None` when nothing is kept
    /// or said.
    fn keep(&self, plan: &Plan, key: &str, run: &GateRun) -> Option<BaseKept> {
        let unkept = |verdict, why| {
            Some(BaseKept {
                verdict,
                recorded: None,
                reused: false,
                commands: key.to_owned(),
                unkept: Some(why),
            })
        };
        let mut checks = match base::read(self.dirs) {
            Ok(checks) => checks,
            Err(why) => return unkept(base::verdict(&run.report).ok(), why),
        };
        if self.place == Place::Gate && !checks.holds(&plan.commit, key) {
            return None;
        }
        let verdict = match base::verdict(&run.report) {
            Ok(verdict) => verdict,
            Err(why) => {
                // A check due that said nothing: its failure takes the
                // branch's path, and no Build waits on it any more.
                if checks.drop_pending(&plan.commit, key)
                    && let Err(error) = base::write(self.dirs, &checks)
                {
                    return unkept(None, format!("{why}; {error}"));
                }
                return unkept(None, why);
            }
        };
        let initiating = match self.place {
            Place::Base => self.initiating,
            Place::Gate => None,
        };
        let recorded = checks.record_check(
            (&plan.commit, key, &plan.head),
            self.now.as_millisecond(),
            verdict.clone(),
            initiating,
        );
        Some(BaseKept {
            verdict: Some(verdict),
            recorded: Some(recorded),
            reused: false,
            commands: key.to_owned(),
            unkept: base::write(self.dirs, &checks).err(),
        })
    }
}

/// What `owlshift gate` prints once the gate ended: each command with the
/// last [`TAIL_LINES`] lines of its output, how it ended and its time; then
/// the verdict, the log, and what could not be cleaned up.
pub fn render(checked: &Checked) -> String {
    let mut out = String::new();
    let Some(run) = &checked.run else {
        out.push_str("No gate command: a Build's gate passes with nothing to run.\n");
        return out;
    };
    let report = &run.report;
    for (step, setup) in report
        .setup
        .iter()
        .map(|step| (step, true))
        .chain(report.steps.iter().map(|step| (step, false)))
    {
        let kind = if setup { "setup " } else { "" };
        out.push_str(&format!("{kind}$ {}\n", step.command));
        let lines: Vec<&str> = step.output.lines().collect();
        let shown = &lines[lines.len().saturating_sub(TAIL_LINES)..];
        if step.truncated || shown.len() < lines.len() {
            out.push_str("    [earlier output in the gate log]\n");
        }
        for line in shown {
            out.push_str(&format!("    {line}\n"));
        }
        let verdict = match &step.failure {
            None => "ok".to_owned(),
            Some(reason) => format!("failed: {reason}"),
        };
        out.push_str(&format!(
            "  {verdict}, {:.1} s\n",
            step.elapsed.as_secs_f64()
        ));
    }
    if !run.violations.is_empty() {
        out.push_str("Isolation broken: the gate changed what a run must leave alone:\n");
        for violation in &run.violations {
            out.push_str(&format!("- {violation}\n"));
        }
        if let Some(marker) = &checked.marker {
            out.push_str(&format!(
                "The project is refused until a person looks: see {}, then clear it with \
                 `owlshift unquarantine`.\n",
                marker.display()
            ));
        }
    } else if let Some(failure) = &report.failure {
        match &failure.command {
            Some(command) => out.push_str(&format!("Gate red: `{command}`: {}.\n", failure.reason)),
            None => out.push_str(&format!("Gate red: {}.\n", failure.reason)),
        }
    } else {
        out.push_str(&format!(
            "Gate green on {}: {} in {:.1} s.\n",
            short(&checked.plan.commit),
            match report.steps.len() {
                1 => "1 command".to_owned(),
                n => format!("{n} commands"),
            },
            checked.elapsed.as_secs_f64()
        ));
    }
    out.push_str(&format!("Gate log: {}\n", report.log.display()));
    if let Some(kept) = &checked.base {
        out.push_str(&hold_line(&checked.plan.commit, kept));
    }
    for warning in &checked.warnings {
        out.push_str(&format!("warning: {warning}\n"));
    }
    out
}

/// A commit id's first 12 characters.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// What `owlshift gate` did to the hold on `commit` (OWL-418).
fn hold_line(commit: &str, kept: &BaseKept) -> String {
    let short = short(commit);
    let mut line = match kept.recorded {
        Some(Recorded::Lifted) => {
            format!("The hold on {short} is lifted: the Builds made from it go on.\n")
        }
        Some(Recorded::StillHeld | Recorded::Opened) => {
            format!("The hold on {short} stands: the Builds made from it still wait.\n")
        }
        Some(Recorded::Cached) => {
            format!("The check due on {short} ran: no Build is held for it.\n")
        }
        None => {
            format!("The hold on {short} stands: this gate's run says nothing of its commands.\n")
        }
    };
    if let Some(why) = &kept.unkept {
        line.push_str(&format!("warning: the base check was not kept: {why}\n"));
    }
    line
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::Path;
    use std::process::Command;

    use owlshift_adapters::forge::Repo;

    use super::*;
    use crate::agent_env::AgentEnv;
    use crate::executor::Git;
    use crate::on_demand;

    /// A project file whose gate is `gate`, a TOML array's items.
    fn project_file(gate: &str) -> String {
        format!(
            "requires = \">=0.0\"\n[tracker]\nkind = \"markdown\"\nadmit = \"delegation\"\n\
             states = {{ ready = \"a\", working = \"b\", needs_input = \"c\", review = \"d\" }}\n\
             [stack]\ngate = [{gate}]\n[pipeline]\ndefault = \"trivial\"\n\
             plan_approval = \"never\"\n[models]\n[policy]\nalways_human = []\n"
        )
    }

    /// Starts the gh the credential check will find, once for this test
    /// binary and off the probes' clocks: its first start on a fresh Windows
    /// runner can outlive the probe's deadline (OWL-56). The test bench's
    /// `gh::warm_up` does the same; the runner cannot depend on the bench.
    fn warm_up_gh(env: &[(OsString, OsString)]) {
        use std::sync::OnceLock;

        use owlshift_platform::process::{OUTPUT_CAP, find_executable_in, run_command};

        static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
        let started = STARTED.get_or_init(|| {
            let path = env
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
                .map(|(_, value)| value.clone())
                .unwrap_or_default();
            let Some(gh) = find_executable_in("gh", path) else {
                return Ok(());
            };
            let mut command = Command::new(&gh);
            command
                .arg("--version")
                .env_clear()
                .envs(env.iter().map(|(n, v)| (n, v)));
            match run_command(&mut command, None, Duration::from_secs(120), OUTPUT_CAP) {
                Ok(captured) if captured.success() => Ok(()),
                outcome => Err(format!("{} --version: {outcome:?}", gh.display())),
            }
        });
        if let Err(error) = started {
            panic!("gh did not start: {error}");
        }
    }

    /// The operator's project file, its gate `commands`.
    fn local_config(commands: &[&str]) -> ProjectConfig {
        let items: Vec<String> = commands
            .iter()
            .map(|command| toml::Value::String((*command).to_owned()).to_string())
            .collect();
        ProjectConfig::parse(&project_file(&items.join(", "))).unwrap()
    }

    /// An origin with a work clone that pushes to it, hermetic git, a home
    /// of its own and a data directory: what `owlshift gate` works with.
    struct Bench {
        _root: tempfile::TempDir,
        root: PathBuf,
        origin: PathBuf,
        work: PathBuf,
        dirs: ProjectDirs,
    }

    impl Bench {
        fn new() -> Self {
            Self::with_root(tempfile::tempdir().unwrap())
        }

        /// A bench whose data directory lies under `/tmp`, as one that
        /// `OWLSHIFT_DATA_DIR` puts there, whatever the system temp folder.
        #[cfg(unix)]
        fn under_tmp() -> Self {
            Self::with_root(tempfile::tempdir_in("/tmp").unwrap())
        }

        fn with_root(tmp: tempfile::TempDir) -> Self {
            // Resolved on Unix, where macOS's temporary folder is a link; not
            // on Windows, where it would take the `\\?\` form git cannot read
            // a configuration file through.
            let root = if cfg!(unix) {
                std::fs::canonicalize(tmp.path()).unwrap()
            } else {
                tmp.path().to_owned()
            };
            std::fs::create_dir_all(root.join("home")).unwrap();
            std::fs::write(
                root.join("gitconfig"),
                "[user]\n\tname = Bench\n\temail = bench@example.invalid\n\
                 [init]\n\tdefaultBranch = main\n[core]\n\tautocrlf = false\n",
            )
            .unwrap();
            let bench = Self {
                origin: root.join("origin.git"),
                work: root.join("work"),
                // Another command holding the lock is answered within a
                // moment rather than the minute a command waits.
                dirs: ProjectDirs::new(&root.join("data"), &Repo::parse("acme/app").unwrap())
                    .with_lock_wait(project::LOCK_GRACE),
                root,
                _root: tmp,
            };
            bench.git(&bench.root, &["init", "--quiet", "--bare", "origin.git"]);
            bench.git(&bench.root, &["clone", "--quiet", "origin.git", "work"]);
            bench
        }

        fn git(&self, dir: &Path, args: &[&str]) -> String {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        }

        /// Commits `owlshift.toml` as `content` (removed when `None`) on
        /// `branch`, pushes it, and returns the commit.
        fn push(&self, branch: &str, content: Option<&str>) -> String {
            let file = self.work.join("owlshift.toml");
            match content {
                Some(content) => std::fs::write(&file, content).unwrap(),
                None => {
                    let _ = std::fs::remove_file(&file);
                }
            }
            self.git(&self.work, &["checkout", "--quiet", "-B", branch]);
            self.git(&self.work, &["add", "-A"]);
            self.git(
                &self.work,
                &["commit", "--quiet", "--allow-empty", "-m", branch],
            );
            self.git(&self.work, &["push", "--quiet", "origin", branch]);
            self.git(&self.work, &["rev-parse", "HEAD"])
        }

        /// The agent environment of a run, from the runner's own with this
        /// bench's home.
        fn agent(&self) -> AgentEnv {
            let parent = std::env::vars_os()
                .filter(|(name, _)| name != "HOME")
                .chain([(OsString::from("HOME"), self.root.join("home").into())]);
            let agent = AgentEnv::new(parent).unwrap();
            warm_up_gh(agent.vars());
            agent
        }

        fn executor(&self, agent: AgentEnv) -> Executor {
            let config = self.root.join("gitconfig");
            let git = Git::with_setup("git", move |command| {
                command
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_CONFIG_GLOBAL", &config);
            });
            on_demand::executor(git, agent)
        }

        fn gate(
            &self,
            executor: &Executor,
            local: &[&str],
            at: Option<&str>,
        ) -> Result<Checked, Stop> {
            let local = local_config(local);
            let origin = self.origin.to_string_lossy().into_owned();
            GateOnce {
                executor,
                dirs: &self.dirs,
                remote_url: &origin,
                local: &local,
                at,
                place: Place::Gate,
                except: None,
                initiating: None,
                now: Timestamp::now(),
            }
            .run(&mut |_| {})
        }

        /// Whether the gate's worktree or its branch is still there.
        fn left_over(&self) -> bool {
            let branch = Command::new("git")
                .args(["show-ref", "--verify", "--quiet"])
                .arg(format!("refs/heads/{GATE_BRANCH}"))
                .current_dir(self.dirs.checkout())
                .status()
                .unwrap();
            fs::symlink_metadata(worktree(&self.dirs)).is_ok() || branch.success()
        }
    }

    /// The gate runs at origin's default branch from its project file, red
    /// at its first failing command; at `--at`'s branch; from the operator's
    /// file at a commit without one; and nothing for an empty gate. Each
    /// leaves no worktree, branch or marker behind.
    #[test]
    fn the_gate_runs_at_the_commit_checked_and_leaves_nothing_behind() {
        let bench = Bench::new();
        let main = bench.push("main", Some(&project_file("\"echo one\", \"exit 3\"")));
        let fixed = bench.push("fixed", Some(&project_file("\"echo fixed\"")));
        bench.push("bare", None);
        // A gate that writes an ignored file and an untracked one.
        std::fs::write(bench.work.join(".gitignore"), "kept.log\n").unwrap();
        let writes = "\"echo x > kept.log\", \"echo x > stray.txt\"";
        bench.push("writes", Some(&project_file(writes)));
        let executor = bench.executor(bench.agent().without_confinement());

        let empty = bench.gate(&executor, &[], Some("origin/bare")).unwrap();
        assert!(empty.passed() && empty.run.is_none(), "{empty:?}");
        assert!(render(&empty).contains("nothing to run"));
        assert!(!bench.dirs.root().join("gate").exists());

        let mut told = None;
        let origin = bench.origin.to_string_lossy().into_owned();
        let red = GateOnce {
            executor: &executor,
            dirs: &bench.dirs,
            remote_url: &origin,
            local: &local_config(&["echo local"]),
            at: None,
            place: Place::Gate,
            except: None,
            initiating: None,
            now: Timestamp::now(),
        }
        .run(&mut |plan| told = Some(plan.clone()))
        .unwrap();
        let told = told.unwrap();
        assert_eq!(
            (told.commit.as_str(), told.name.as_str(), told.source),
            (main.as_str(), "origin/main", Source::Commit)
        );
        assert_eq!(told.commands, ["echo one", "exit 3"]);
        let run = red.run.as_ref().unwrap();
        assert!(!red.passed());
        let failure = run.report.failure.as_ref().unwrap();
        assert_eq!(failure.command.as_deref(), Some("exit 3"));
        let steps = &run.report.steps;
        assert_eq!(steps.len(), 2);
        assert!(steps[0].failure.is_none() && steps[0].output.contains("one"));
        assert_eq!(steps[1].failure.as_deref(), Some("exit status 3"));
        let text = render(&red);
        assert!(
            text.contains("$ echo one\n    one")
                && text.contains("Gate red: `exit 3`: exit status 3.")
                && text.contains("gate.log"),
            "{text}"
        );
        assert!(!bench.left_over());
        assert_eq!(bench.dirs.quarantine().unwrap(), None);
        assert!(!bench.dirs.obligations_stand());

        let green = bench.gate(&executor, &[], Some("origin/fixed")).unwrap();
        assert!(green.passed(), "{}", render(&green));
        assert_eq!(green.plan.commit, fixed);
        assert!(render(&green).contains("Gate green on"));
        let local = bench
            .gate(&executor, &["echo local"], Some("origin/bare"))
            .unwrap();
        assert!(local.passed(), "{}", render(&local));
        assert_eq!(local.plan.source, Source::Local);
        assert!(!bench.left_over());

        // As a Build's gate: an untracked file the gate leaves fails it, an
        // ignored one does not.
        let wrote = bench.gate(&executor, &[], Some("origin/writes")).unwrap();
        let failure = wrote.run.as_ref().unwrap().report.failure.clone().unwrap();
        assert_eq!(failure.command, None);
        // A machine with git-lfs configured adds a note on the drivers.
        assert!(
            failure.reason == "the gate changed files: stray.txt"
                || failure
                    .reason
                    .starts_with("the gate changed files: stray.txt (read with the filter"),
            "{}",
            failure.reason
        );
        assert!(!bench.left_over());

        let unknown = bench
            .gate(&executor, &[], Some("origin/nowhere"))
            .unwrap_err();
        assert!(
            matches!(&unknown, Stop::Refused(reason) if reason.starts_with("--at")),
            "{unknown:?}"
        );
    }

    /// Refused while another command holds the lock; a credential within
    /// the agent's reach refuses the gate and cleans up; a breach marks the
    /// project and keeps the worktree, and the next gate is refused until a
    /// person removes the marker, then removes what was kept.
    #[test]
    fn a_breach_refuses_the_project_until_a_person_looks() {
        let bench = Bench::new();
        bench.push(
            "main",
            Some(&project_file("\"git config --local owl.planted yes\"")),
        );
        bench.push("fixed", Some(&project_file("\"echo fixed\"")));
        let executor = bench.executor(bench.agent().without_confinement());

        let held = bench.dirs.lock().unwrap().unwrap();
        assert!(matches!(
            bench.gate(&executor, &[], None),
            Err(Stop::Busy(_))
        ));
        drop(held);
        // A step in flight holds its ticket's lock, not the project's
        // (OWL-401): the gate answers busy all the same.
        let ticket = owlshift_contracts::ids::TicketId::new("OWL-1").unwrap();
        let step = bench.dirs.ticket_lock(&ticket).unwrap().unwrap();
        assert!(matches!(
            bench.gate(&executor, &[], None),
            Err(Stop::Busy(_))
        ));
        drop(step);

        // A header git would send, set in the checkout the worktree shares.
        let header = ["config", "http.extraHeader", "AUTHORIZATION: basic eA=="];
        bench.gate(&executor, &[], Some("origin/fixed")).unwrap();
        bench.git(&bench.dirs.checkout(), &header);
        let refused = bench
            .gate(&executor, &[], Some("origin/fixed"))
            .unwrap_err();
        assert!(
            matches!(&refused, Stop::Refused(reason) if reason.contains("could not start")),
            "{refused:?}"
        );
        assert!(!bench.left_over());
        assert_eq!(bench.dirs.quarantine().unwrap(), None);
        assert!(!bench.dirs.obligations_stand());
        bench.git(
            &bench.dirs.checkout(),
            &["config", "--unset", "http.extraHeader"],
        );

        let breached = bench.gate(&executor, &[], None).unwrap();
        assert!(!breached.passed());
        assert!(!breached.run.as_ref().unwrap().violations.is_empty());
        assert_eq!(breached.marker, Some(bench.dirs.quarantine_file()));
        assert!(bench.dirs.obligations_stand(), "its obligation stays");
        assert!(render(&breached).contains("Isolation broken"));
        assert!(bench.left_over());
        let marker = bench.dirs.quarantine().unwrap().unwrap();
        assert!(marker.contains("broke isolation"), "{marker}");

        let again = bench
            .gate(&executor, &[], Some("origin/fixed"))
            .unwrap_err();
        assert!(matches!(again, Stop::Unverified { .. }), "{again:?}");
        assert!(bench.left_over());

        // The person looked.
        std::fs::remove_file(bench.dirs.quarantine_file()).unwrap();
        std::fs::remove_dir_all(bench.dirs.obligations_dir()).unwrap();
        let green = bench.gate(&executor, &[], Some("origin/fixed")).unwrap();
        assert!(green.passed(), "{}", render(&green));
        assert!(!bench.left_over());
    }

    /// OWL-330: `owlshift gate` runs the commit's setup before its gate, in
    /// the same temporary folder, lists it first and fails on a red one; a
    /// setup alone is no gate.
    #[test]
    fn the_gate_runs_the_setup_first_and_fails_on_a_red_one() {
        let bench = Bench::new();
        let with = |gate: &str, setup: &str| project_file(&format!("{gate}]\nsetup = [{setup}"));
        // `cmd`'s on Windows, which expands `%TMPDIR%` and not `$TMPDIR`.
        let (gate, setup) = if cfg!(windows) {
            (r#"type "%TMPDIR%\x""#, r#"type nul > "%TMPDIR%\x""#)
        } else {
            (r#"test -f "$TMPDIR/x""#, r#"touch "$TMPDIR/x""#)
        };
        let literal = |line: &str| format!("'{line}'");
        bench.push("main", Some(&with(&literal(gate), &literal(setup))));
        bench.push(
            "red",
            Some(&with(r#""echo never""#, r#""echo partial && exit 4""#)),
        );
        bench.push("alone", Some(&with("", r#""true""#)));
        let executor = bench.executor(bench.agent().without_confinement());

        let mut told = None;
        let origin = bench.origin.to_string_lossy().into_owned();
        let green = GateOnce {
            executor: &executor,
            dirs: &bench.dirs,
            remote_url: &origin,
            local: &local_config(&["true"]),
            at: None,
            place: Place::Gate,
            except: None,
            initiating: None,
            now: Timestamp::now(),
        }
        .run(&mut |plan| told = Some(plan.clone()))
        .unwrap();
        assert!(green.passed(), "{}", render(&green));
        let told = told.unwrap();
        assert_eq!(told.setup, [setup]);
        let shown = told.to_string();
        let setup_at = shown.find("after its setup:").unwrap();
        assert!(setup_at < shown.find(gate).unwrap(), "{shown}");
        let text = render(&green);
        let setup_at = text.find(&format!("setup $ {setup}")).unwrap();
        assert!(
            setup_at < text.find(&format!("$ {gate}")).unwrap(),
            "{text}"
        );

        let red = bench.gate(&executor, &[], Some("origin/red")).unwrap();
        assert!(!red.passed());
        let report = &red.run.as_ref().unwrap().report;
        let failure = report.failure.as_ref().unwrap();
        assert_eq!(failure.command.as_deref(), Some("echo partial && exit 4"));
        assert_eq!(failure.reason, "setup: exit status 4");
        assert!(report.steps.is_empty());
        assert!(
            render(&red).contains("Gate red: `echo partial && exit 4`: setup: exit status 4."),
            "{}",
            render(&red)
        );

        let alone = bench.gate(&executor, &[], Some("origin/alone")).unwrap();
        assert!(alone.passed() && alone.run.is_none(), "{alone:?}");
        assert!(!bench.left_over());
    }

    /// OWL-330: a commit whose `stack.caches` differ from the operator's file
    /// is noted, as a Build warns of it (`caches_differ`): the gate reads the
    /// caches the operator's file declares.
    #[test]
    fn caches_that_differ_from_the_operators_file_are_noted() {
        let bench = Bench::new();
        bench.push(
            "main",
            Some(&project_file("\"true\"]\ncaches = [\"Library/pnpm\"")),
        );
        let executor = bench.executor(bench.agent().without_confinement());
        let checked = bench.gate(&executor, &["true"], None).unwrap();
        assert!(checked.passed(), "{}", render(&checked));
        assert_eq!(
            checked.plan.caches_differ,
            Some(vec!["Library/pnpm".to_owned()])
        );
        assert!(
            checked.plan.to_string().contains(
                "Note: the commit's stack.caches declares [Library/pnpm]; the gate reads the \
                 caches your owlshift.toml declares"
            ),
            "{}",
            checked.plan
        );
        bench.push("main", Some(&project_file("\"true\"")));
        let checked = bench.gate(&executor, &["true"], None).unwrap();
        assert_eq!(checked.plan.caches_differ, None);
    }

    /// Confined as a Build's gate: the home's files and the gate log are out
    /// of reach, `TMPDIR` is the run's own folder, and what one command
    /// leaves there is seen by the next exactly as in a Build, on Linux as on
    /// macOS: bwrap binds the folder itself, even inside its private `/tmp`
    /// (OWL-330).
    #[cfg(unix)]
    #[test]
    fn the_gate_is_confined_as_a_builds() {
        confined_as_a_builds(Bench::new);
    }

    /// The same with the data directory, and so the worktree and the gate
    /// log, under `/tmp`, as `OWLSHIFT_DATA_DIR` may put them: inside the
    /// private `/tmp` bwrap mounts, into which the run's folders are bound
    /// back (OWL-330).
    #[cfg(unix)]
    #[test]
    fn the_gate_is_confined_as_a_builds_with_its_data_under_tmp() {
        confined_as_a_builds(Bench::under_tmp);
    }

    #[cfg(unix)]
    fn confined_as_a_builds(bench: fn() -> Bench) {
        if let Err(error) = owlshift_platform::sandbox::available() {
            assert!(
                std::env::var_os("OWLSHIFT_REQUIRE_CONFINEMENT").is_none(),
                "confinement is required here, but: {error}"
            );
            eprintln!("skipped: agent runs cannot be confined here: {error}");
            return;
        }
        let bench = bench();
        bench.push("main", None);
        std::fs::write(bench.root.join("home/secret.txt"), "FAKE_owl329\n").unwrap();
        let log = run_dir(&bench.dirs).join(GATE_LOG);
        let first = format!(
            "echo probe > \"$TMPDIR/probe\"; echo \"tmp=$TMPDIR\"; cat \"$HOME/secret.txt\"; cat '{}'; true",
            log.display()
        );
        let executor = bench.executor(bench.agent());
        let checked = bench
            .gate(&executor, &[&first, "ls -a \"$TMPDIR\""], None)
            .unwrap();
        assert!(checked.passed(), "{}", render(&checked));
        let steps = &checked.run.as_ref().unwrap().report.steps;
        assert!(steps[0].output.contains("tmp=") && steps[0].output.contains("owlshift-run-"));
        assert!(
            !steps[0].output.contains("FAKE_owl329"),
            "{}",
            steps[0].output
        );
        assert!(
            !steps[0].output.contains("$ echo probe"),
            "{}",
            steps[0].output
        );
        assert!(steps[1].output.contains("probe"), "{}", steps[1].output);
    }

    /// OWL-418: a base check keeps its verdict, matched with the failure it
    /// ran for, and is used again at the same head with nothing run; since
    /// OWL-660 it is busy while another base check holds the base lock, and
    /// runs while another ticket's step is in flight, its record left with
    /// the base branch gone; the operator's gate keeps nothing on a commit
    /// nothing holds, and its pass lifts a hold.
    #[test]
    fn a_base_check_is_kept_once_and_the_operators_gate_lifts_a_hold() {
        use owlshift_core::base::{Checks, Failure, Step};
        let bench = Bench::new();
        let main = bench.push("main", Some(&project_file("\"exit 3\"")));
        let fixed = bench.push("fixed", Some(&project_file("\"echo fixed\"")));
        let executor = bench.executor(bench.agent().without_confinement());
        let origin = bench.origin.to_string_lossy().into_owned();
        let local = local_config(&["true"]);
        let own = TicketId::new("OWL-1").unwrap();
        let failing = Failing {
            step: Step::Gate,
            command: "exit 3".to_owned(),
        };
        let base_check = || {
            GateOnce {
                executor: &executor,
                dirs: &bench.dirs,
                remote_url: &origin,
                local: &local,
                at: Some(main.as_str()),
                place: Place::Base,
                except: Some(&own),
                initiating: Some(&failing),
                now: Timestamp::now(),
            }
            .run(&mut |_| {})
        };
        let own_lock = bench.dirs.ticket_lock(&own).unwrap().unwrap();
        let other_check = bench.dirs.base_lock().unwrap().unwrap();
        assert!(matches!(base_check(), Err(Stop::Busy(_))));
        drop(other_check);
        let other = TicketId::new("OWL-2").unwrap();
        let sibling = bench.dirs.ticket_lock(&other).unwrap().unwrap();

        let checked = base_check().unwrap();
        drop(sibling);
        assert!(checked.run.is_some(), "{checked:?}");
        let record = ticket_run::read_base_run(&bench.dirs).unwrap();
        assert_eq!(record.tip, None, "the base branch is gone");
        assert!(record.entry.is_some());
        let kept = checked.base.unwrap();
        assert_eq!(
            (kept.recorded, kept.reused),
            (Some(Recorded::Opened), false)
        );
        assert!(!bench.left_over() && !worktree_at(&bench.dirs, Place::Base).exists());
        let again = base_check().unwrap();
        assert!(again.run.is_none());
        let kept = again.base.unwrap();
        assert_eq!(
            (kept.recorded, kept.reused),
            (Some(Recorded::StillHeld), true)
        );
        // The operator's gate runs with no ticket's lock held.
        drop(own_lock);

        let unheld = bench.gate(&executor, &[], Some(&fixed)).unwrap();
        assert!(unheld.base.is_none(), "{unheld:?}");

        // A hold on `fixed`, whose gate passes now: the operator's gate lifts it.
        let key = base::commands_key(&[], &["echo fixed".to_owned()]);
        let mut checks = base::read(&bench.dirs).unwrap();
        let held = Failing {
            step: Step::Gate,
            command: "echo fixed".to_owned(),
        };
        checks.record_check(
            (&fixed, &key, "an older head"),
            0,
            Verdict::Failed(Failure {
                failing: held.clone(),
                reason: "exit status 1".to_owned(),
                tail: String::new(),
            }),
            Some(&held),
        );
        base::write(&bench.dirs, &checks).unwrap();
        let lifted = bench.gate(&executor, &[], Some(&fixed)).unwrap();
        assert_eq!(
            lifted.base.as_ref().unwrap().recorded,
            Some(Recorded::Lifted)
        );
        assert!(render(&lifted).contains("is lifted"), "{}", render(&lifted));
        let checks: Checks = base::read(&bench.dirs).unwrap();
        assert!(!checks.holds_commit(&fixed));
        assert!(checks.holds_commit(&main));
    }

    /// A gate command that writes `started` in `dir`, then waits there for
    /// `go` before it fails: a base check held under way as long as a test
    /// needs (OWL-660).
    #[cfg(unix)]
    fn held_gate(dir: &Path) -> String {
        let (started, go) = (dir.join("started"), dir.join("go"));
        format!(
            "touch '{}'; while [ ! -e '{}' ]; do sleep 0.05; done; exit 3",
            started.display(),
            go.display()
        )
    }

    /// Writes the `go` file of [`held_gate`] when dropped, so a held gate
    /// ends even when an assertion fails before the test lets it go.
    #[cfg(unix)]
    struct Release(PathBuf);

    #[cfg(unix)]
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, "");
        }
    }

    /// Waits for `file` to be written, a minute at most.
    #[cfg(unix)]
    fn wait_for(file: &Path) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !file.exists() {
            assert!(Instant::now() < deadline, "{} never came", file.display());
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A ticket whose step stands for one in flight: its run file, as a
    /// step's start leaves it, and its lock, held until dropped.
    fn in_flight(dirs: &ProjectDirs, ticket: &TicketId) -> crate::project::TicketLock {
        let run = owlshift_contracts::step::TicketRun::new(ticket.clone());
        fs::create_dir_all(dirs.ticket_dir(ticket)).unwrap();
        fs::write(dirs.run_file(ticket), run.render()).unwrap();
        dirs.ticket_lock(ticket).unwrap().unwrap()
    }

    /// The refs a list of violations names.
    fn refs_named(violations: &[crate::executor::isolation::Violation]) -> Vec<String> {
        violations
            .iter()
            .flat_map(|violation| match violation {
                crate::executor::isolation::Violation::Refs(refs) => refs.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    /// OWL-660, criteria 2 and 3: a base check runs beside another ticket's
    /// step. While its gate runs, the project's lock is free, the base lock
    /// is held and its obligation, named for its ticket, reads as in flight.
    /// The sibling's branch and worktree, made meanwhile under its lock, are
    /// no breach of the check; the base branch and entry, removed once it
    /// ended, are none of the sibling's. A real breach during a second check,
    /// a tag planted and the checking ticket's own branch made, is caught by
    /// both: the base check names both, the sibling's check the tag alone.
    #[cfg(unix)]
    #[test]
    fn a_base_check_runs_beside_the_steps_and_a_breach_is_caught_on_both_sides() {
        use crate::executor::isolation::{Snapshot, Violation};

        let bench = Bench::new();
        let hold = bench.root.join("hold");
        fs::create_dir_all(&hold).unwrap();
        let gate = held_gate(&hold);
        let main = bench.push(
            "main",
            Some(&project_file(
                &toml::Value::String(gate.clone()).to_string(),
            )),
        );
        let executor = bench.executor(bench.agent().without_confinement());
        let git = executor.git.clone();
        let origin = bench.origin.to_string_lossy().into_owned();
        let local = local_config(&["true"]);
        let own = TicketId::new("OWL-1").unwrap();
        let other = TicketId::new("OWL-2").unwrap();
        let failing = Failing {
            step: owlshift_core::base::Step::Gate,
            command: gate,
        };
        let _own = in_flight(&bench.dirs, &own);
        let _other = in_flight(&bench.dirs, &other);
        let checkout = bench.dirs.checkout();
        let theirs = bench.root.join("theirs");
        let theirs_path = theirs.to_string_lossy().into_owned();
        let own_branch = crate::on_demand::branch_for(&own);
        let base_check = || {
            GateOnce {
                executor: &executor,
                dirs: &bench.dirs,
                remote_url: &origin,
                local: &local,
                at: Some(main.as_str()),
                place: Place::Base,
                except: Some(&own),
                initiating: Some(&failing),
                now: Timestamp::now(),
            }
            .run(&mut |_| {})
        };
        let siblings = Siblings::new(&bench.dirs, &other);

        // The first check, clean.
        let (checked, before, marks) = std::thread::scope(|scope| {
            let check = scope.spawn(base_check);
            let release = Release(hold.join("go"));
            wait_for(&hold.join("started"));
            assert!(
                bench.dirs.try_lock().unwrap().is_some(),
                "the project's lock is free"
            );
            assert!(bench.dirs.base_lock_held().unwrap());
            let names: Vec<String> = ticket_run::obligations(&bench.dirs)
                .unwrap()
                .into_iter()
                .map(|found| found.name)
                .collect();
            assert!(
                matches!(names.as_slice(), [name] if name.starts_with("owl-1-") && name.ends_with("-base")),
                "{names:?}"
            );
            assert_eq!(
                ticket_run::look(&bench.dirs, None).unwrap(),
                Look::Clear { running: true }
            );
            // The sibling makes its branch and worktree, its lock held, and
            // takes its snapshot while the base branch and entry stand.
            bench.git(
                &checkout,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    "owlshift/owl-2",
                    &theirs_path,
                    &main,
                ],
            );
            let marks = siblings.marks().unwrap();
            let before = Snapshot::take(&git, &checkout, &theirs, "owlshift/owl-2").unwrap();
            drop(release);
            (check.join().unwrap(), before, marks)
        });
        let checked = checked.unwrap();
        assert!(checked.marker.is_none(), "{checked:?}");
        assert!(
            checked.run.as_ref().unwrap().violations.is_empty(),
            "{checked:?}"
        );
        assert_eq!(
            checked.base.as_ref().unwrap().recorded,
            Some(Recorded::Opened)
        );
        assert!(!worktree_at(&bench.dirs, Place::Base).exists());
        assert!(!bench.dirs.obligations_stand());
        let read = || siblings.concurrent(&marks);
        let theirs_checked = before.check_among(&git, &checkout, &theirs, "owlshift/owl-2", &read);
        assert!(
            theirs_checked.violations.is_empty(),
            "{:?}",
            theirs_checked.violations
        );

        // The second check, at a new head so nothing is reused: a breach.
        fs::remove_file(hold.join("started")).unwrap();
        fs::remove_file(hold.join("go")).unwrap();
        bench.push("main", Some(&project_file("\"true\"")));
        let (checked, before, marks) = std::thread::scope(|scope| {
            let check = scope.spawn(base_check);
            let release = Release(hold.join("go"));
            wait_for(&hold.join("started"));
            let marks = siblings.marks().unwrap();
            let before = Snapshot::take(&git, &checkout, &theirs, "owlshift/owl-2").unwrap();
            // The sibling commits on its branch, its lock held.
            bench.git(
                &theirs,
                &["commit", "--quiet", "--allow-empty", "-m", "theirs"],
            );
            bench.git(&checkout, &["tag", "planted", &main]);
            bench.git(&checkout, &["branch", &own_branch, &main]);
            drop(release);
            (check.join().unwrap(), before, marks)
        });
        let checked = checked.unwrap();
        assert!(checked.marker.is_some(), "{checked:?}");
        let violations = &checked.run.as_ref().unwrap().violations;
        assert_eq!(
            refs_named(violations),
            [
                format!("refs/heads/{own_branch}"),
                "refs/tags/planted".to_owned()
            ],
            "{violations:?}"
        );
        assert!(
            !violations
                .iter()
                .any(|v| matches!(v, Violation::WorktreeEntries(_))),
            "{violations:?}"
        );
        let read = || siblings.concurrent(&marks);
        let theirs_checked = before.check_among(&git, &checkout, &theirs, "owlshift/owl-2", &read);
        assert_eq!(
            refs_named(&theirs_checked.violations),
            ["refs/tags/planted"],
            "{:?}",
            theirs_checked.violations
        );
        assert!(bench.dirs.quarantine().unwrap().is_some());
    }

    /// OWL-660: a base check whose last section cannot be had, the project's
    /// lock held past its bound once its gate ended, leaves no obligation and
    /// no quarantine, keeps no verdict and leaves its worktree and branch,
    /// which its record names; the next check finds them by their entry's
    /// `gitdir`, though no record names it any more, removes them and runs.
    #[cfg(unix)]
    #[test]
    fn a_base_check_whose_last_section_is_refused_keeps_nothing_and_leaves_its_worktree() {
        let bench = Bench::new();
        let hold = bench.root.join("hold");
        fs::create_dir_all(&hold).unwrap();
        let gate = held_gate(&hold);
        let main = bench.push(
            "main",
            Some(&project_file(
                &toml::Value::String(gate.clone()).to_string(),
            )),
        );
        let executor = bench.executor(bench.agent().without_confinement());
        let origin = bench.origin.to_string_lossy().into_owned();
        let local = local_config(&["true"]);
        let own = TicketId::new("OWL-1").unwrap();
        let failing = Failing {
            step: owlshift_core::base::Step::Gate,
            command: gate,
        };
        let _own = in_flight(&bench.dirs, &own);
        let base_check = || {
            GateOnce {
                executor: &executor,
                dirs: &bench.dirs,
                remote_url: &origin,
                local: &local,
                at: Some(main.as_str()),
                place: Place::Base,
                except: Some(&own),
                initiating: Some(&failing),
                now: Timestamp::now(),
            }
            .run(&mut |_| {})
        };
        let checked = std::thread::scope(|scope| {
            let check = scope.spawn(base_check);
            let release = Release(hold.join("go"));
            wait_for(&hold.join("started"));
            let held = bench.dirs.lock().unwrap().unwrap();
            drop(release);
            let checked = check.join().unwrap();
            drop(held);
            checked
        });
        let checked = checked.unwrap();
        assert!(checked.marker.is_none(), "{checked:?}");
        let kept = checked.base.as_ref().unwrap();
        assert_eq!(kept.recorded, None);
        assert!(
            kept.unkept.as_ref().unwrap().contains("lock stayed held"),
            "{kept:?}"
        );
        assert!(!bench.dirs.obligations_stand());
        assert_eq!(bench.dirs.quarantine().unwrap(), None);
        assert!(
            base::read(&bench.dirs)
                .unwrap()
                .get(&main, &kept.commands)
                .is_none()
        );
        let worktree = worktree_at(&bench.dirs, Place::Base);
        assert!(worktree.exists());
        let record = ticket_run::read_base_run(&bench.dirs).unwrap();
        assert_eq!(record.tip.as_deref(), Some(main.as_str()));

        // No record names the entry any more: its `gitdir` does.
        fs::remove_file(bench.dirs.base_run_file()).unwrap();
        let entries = ticket_run::entries_of(&bench.dirs.checkout(), &worktree);
        assert_eq!(entries.len(), 1, "{entries:?}");
        let again = base_check().unwrap();
        assert!(again.run.is_some(), "{again:?}");
        assert_eq!(
            again.base.as_ref().unwrap().recorded,
            Some(Recorded::Opened),
            "{again:?}"
        );
        assert!(!worktree.exists());
        assert!(ticket_run::entries_of(&bench.dirs.checkout(), &worktree).is_empty());
    }

    /// Native Windows has no sandbox (D9): refused before anything is made.
    #[cfg(windows)]
    #[test]
    fn the_gate_is_refused_on_native_windows() {
        let data = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(data.path(), &Repo::parse("acme/app").unwrap());
        let executor =
            on_demand::executor(Git::new("git"), AgentEnv::new(std::env::vars_os()).unwrap());
        let refused = GateOnce {
            executor: &executor,
            dirs: &dirs,
            remote_url: "https://github.com/acme/app.git",
            local: &local_config(&["echo hi"]),
            at: None,
            place: Place::Gate,
            except: None,
            initiating: None,
            now: Timestamp::now(),
        }
        .run(&mut |_| panic!("nothing runs"))
        .unwrap_err();
        assert!(
            matches!(&refused, Stop::Refused(reason) if reason.contains("WSL2")),
            "{refused:?}"
        );
        assert!(!dirs.checkout().exists() && !worktree(&dirs).exists());
    }
}
