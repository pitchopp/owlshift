//! `owlshift gate` (OWL-329): the project's gate, run once, alone, as a
//! Build's gate runs it, so an operator can try a gate in seconds rather
//! than in a Build run and a park.
//!
//! [`GateOnce::run`] takes what a Build's preparation takes, in its order:
//! git's minimum version, the project's lock, the `unverified` marker, the
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

use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use jiff::Timestamp;

use crate::executor::gate::GATE_LOG;
use crate::executor::{Executor, GateRun, GateSpec};
use owlshift_contracts::config::ProjectConfig;

use crate::project::{self, Base, ProjectDirs};

/// The branch of `owlshift gate`'s worktree: no ticket's branch
/// (`owlshift/<id>`) can take it.
pub const GATE_BRANCH: &str = "owlshift-gate/check";

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
    pub commands: Vec<String>,
    /// The gate log.
    pub log: PathBuf,
    /// The names the commit's `stack.gate_env` declares, when they differ
    /// from those the agent environment passes, the operator's: a Build
    /// warns of it too (`gate_env_differs`).
    pub gate_env_differs: Option<Vec<String>>,
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
        writeln!(f, "Its whole output goes to {}.", self.log.display())
    }
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
        }
    }
}

/// The worktree of `owlshift gate` under the project's folder.
pub fn worktree(dirs: &ProjectDirs) -> PathBuf {
    dirs.root().join("gate").join("worktree")
}

/// The folder of its log, hidden from the gate.
pub fn run_dir(dirs: &ProjectDirs) -> PathBuf {
    dirs.root().join("gate").join("run")
}

impl GateOnce<'_> {
    /// Runs the gate; see the module documentation. `starting` is told what
    /// runs just before it does.
    pub fn run(&self, starting: &mut dyn FnMut(&Plan)) -> Result<Checked, Stop> {
        let started = Instant::now();
        let executor = self.executor;
        let git = &executor.git;
        let dirs = self.dirs;
        // As a Build's preparation: the version, the lock, then the marker,
        // then the confinement (`on_demand`'s `prepare`).
        git.require_minimum_version().map_err(Stop::Refused)?;
        let _lock = match dirs.lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => return Err(Stop::Busy(dirs.root().to_owned())),
            Err(error) => return Err(Stop::Refused(format!("the project's lock: {error}"))),
        };
        match dirs.unverified() {
            Ok(None) => {}
            Ok(Some(text)) => {
                return Err(Stop::Unverified {
                    marker: dirs.unverified_file(),
                    text,
                });
            }
            Err(error) => return Err(Stop::Refused(format!("the project's marker: {error}"))),
        }
        executor
            .agent
            .sandbox_ready()
            .map_err(|e| Stop::Refused(e.to_string()))?;
        let checkout = dirs.checkout();
        let worktree = worktree(dirs);
        // What a stopped `owlshift gate` left, once its marker is gone.
        if fs::symlink_metadata(&checkout).is_ok() {
            executor
                .discard_worktree(&checkout, &worktree, GATE_BRANCH)
                .map_err(|e| {
                    Stop::Refused(format!(
                        "removing what an earlier `owlshift gate` left: {e}"
                    ))
                })?;
        }
        let base = project::sync_checkout(git, dirs, self.remote_url).map_err(Stop::Refused)?;
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
        let commands = config.stack.gate.clone();
        let run_dir = run_dir(dirs);
        let plan = Plan {
            commit,
            name,
            source,
            commands,
            log: run_dir.join(GATE_LOG),
            gate_env_differs,
        };
        let checked = |run, marker, warnings| Checked {
            plan: plan.clone(),
            run,
            elapsed: started.elapsed(),
            marker,
            warnings,
        };
        if plan.commands.is_empty() {
            return Ok(checked(None, None, Vec::new()));
        }

        // As for a run: from here until the isolation check passes, a stop
        // leaves the project refused.
        dirs.mark_unverified(&format!(
            "`owlshift gate` started at {} on {}; its isolation check has not passed. If it was \
             stopped, look at {} before removing this file.\n",
            Timestamp::now(),
            plan.commit,
            worktree.display()
        ))
        .map_err(|e| Stop::Refused(format!("the project's marker: {e}")))?;
        starting(&plan);
        let spec = GateSpec {
            main: &checkout,
            worktree: &worktree,
            branch: GATE_BRANCH,
            base: &plan.commit,
            run_dir: &run_dir,
            commands: &plan.commands,
        };
        let discard = || executor.discard_worktree(&checkout, &worktree, GATE_BRANCH);
        let run = match executor.run_gate(&spec) {
            Ok(run) => run,
            Err(error) => {
                // No gate command ran: nothing to verify, as for a run.
                let mut reason = format!("the gate could not start: {error}");
                if let Err(e) = discard() {
                    reason.push_str(&format!("; its worktree could not be removed: {e}"));
                }
                if let Err(e) = dirs.clear_unverified() {
                    reason.push_str(&format!("; the project's marker: {e}"));
                }
                return Err(Stop::Refused(reason));
            }
        };
        let mut warnings = Vec::new();
        if run.violations.is_empty() {
            if let Err(e) = discard() {
                warnings.push(format!("the gate's worktree could not be removed: {e}"));
            }
            if let Err(e) = dirs.clear_unverified() {
                warnings.push(format!("the project's marker could not be cleared: {e}"));
            }
            return Ok(checked(Some(run), None, warnings));
        }
        // The worktree stays for the person who looks; no more git runs.
        let lines: Vec<String> = run.violations.iter().map(|v| format!("- {v}")).collect();
        // Its time makes the marker unique, as a run's are (OWL-368).
        let text = format!(
            "`owlshift gate` on {} broke isolation at {}; its worktree is kept at {}:\n{}\n",
            plan.commit,
            Timestamp::now(),
            worktree.display(),
            lines.join("\n")
        );
        if let Err(e) = dirs.mark_unverified(&text) {
            warnings.push(format!("the breach could not be recorded: {e}"));
        }
        Ok(checked(Some(run), Some(dirs.unverified_file()), warnings))
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
    for step in &report.steps {
        out.push_str(&format!("$ {}\n", step.command));
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
    for warning in &checked.warnings {
        out.push_str(&format!("warning: {warning}\n"));
    }
    out
}

/// A commit id's first 12 characters.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
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
            let tmp = tempfile::tempdir().unwrap();
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
                dirs: ProjectDirs::new(&root.join("data"), &Repo::parse("acme/app").unwrap()),
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
        assert_eq!(bench.dirs.unverified().unwrap(), None);

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
        assert_eq!(bench.dirs.unverified().unwrap(), None);
        bench.git(
            &bench.dirs.checkout(),
            &["config", "--unset", "http.extraHeader"],
        );

        let breached = bench.gate(&executor, &[], None).unwrap();
        assert!(!breached.passed());
        assert!(!breached.run.as_ref().unwrap().violations.is_empty());
        assert_eq!(breached.marker, Some(bench.dirs.unverified_file()));
        assert!(render(&breached).contains("Isolation broken"));
        assert!(bench.left_over());
        let marker = bench.dirs.unverified().unwrap().unwrap();
        assert!(marker.contains("broke isolation"), "{marker}");

        let again = bench
            .gate(&executor, &[], Some("origin/fixed"))
            .unwrap_err();
        assert!(matches!(again, Stop::Unverified { .. }), "{again:?}");
        assert!(bench.left_over());

        // The person looked.
        std::fs::remove_file(bench.dirs.unverified_file()).unwrap();
        let green = bench.gate(&executor, &[], Some("origin/fixed")).unwrap();
        assert!(green.passed(), "{}", render(&green));
        assert!(!bench.left_over());
    }

    /// Confined as a Build's gate: the home's files and the gate log are out
    /// of reach, `TMPDIR` is the run's own folder, and what one command
    /// leaves there is seen by the next exactly as in a Build, on Linux as on
    /// macOS: bwrap binds the folder itself, even inside its private `/tmp`
    /// (OWL-330).
    #[cfg(unix)]
    #[test]
    fn the_gate_is_confined_as_a_builds() {
        if let Err(error) = owlshift_platform::sandbox::available() {
            assert!(
                std::env::var_os("OWLSHIFT_REQUIRE_CONFINEMENT").is_none(),
                "confinement is required here, but: {error}"
            );
            eprintln!("skipped: agent runs cannot be confined here: {error}");
            return;
        }
        let bench = Bench::new();
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
