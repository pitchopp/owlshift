//! The runner's own git: the commands the executor runs for itself (the
//! worktree, the isolation check, the checks around the gate), never for an
//! agent. The gate's checks run with the agent environment
//! ([`Git::as_agent`]).
//!
//! Each command runs through `owlshift_platform::process::run_command`, as
//! the root of a process tree stopped at [`GIT_TIMEOUT`]. Its output is kept
//! whole ([`WHOLE_OUTPUT`]): the probes' 64 KiB would cut a large
//! repository's status and blind the isolation check.
//!
//! No command runs a hook or a file-system monitor, whatever the
//! repository's configuration says ([`hardening`]): a run can write the
//! repository's shared git files, and the runner's git must not run what it
//! planted there before the isolation check has seen it. Nor does git's own
//! housekeeping, which a `fetch` starts in the background, write
//! `info/refs`, one of those files (OWL-197).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use owlshift_platform::process::{Captured, RunError, run_command};

use crate::agent_env::AgentEnv;
use crate::config::exit_text;

/// The oldest git Owlshift runs with (OWL-199), which `owlshift doctor`
/// checks. Set by `symbolic-ref --no-recurse`, new in 2.39.0, which every
/// `owlshift do` runs to read origin's default branch
/// (`project::sync_checkout`). The other features with a floor are older:
/// `repack.updateServerInfo` (the hardening below, and the agent environment)
/// came in 2.36.0, `GIT_CONFIG_COUNT` and `rev-parse --path-format` in
/// 2.31.0. The live checks behind each: `docs/design/build-plan.md`, "What
/// `doctor` checks".
pub const MINIMUM_GIT_VERSION: &str = "2.39.0";

/// How long one of the executor's git commands may take.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// The output cap of the executor's git commands: none.
const WHOLE_OUTPUT: usize = usize::MAX;

/// How the executor runs git: a program, and what to set on each command.
#[derive(Clone)]
pub struct Git {
    program: PathBuf,
    /// The deadline of a command that names none: [`GIT_TIMEOUT`].
    timeout: Duration,
    setup: Arc<dyn Fn(&mut Command) + Send + Sync>,
}

impl fmt::Debug for Git {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Git")
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl Git {
    /// `program`, found on the runner's `PATH` when it is a bare name, with
    /// the runner's own environment.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self::with_setup(program, |_| {})
    }

    /// `program`, with `setup` applied to every command before it runs; a
    /// test bench makes git hermetic this way.
    pub fn with_setup(
        program: impl Into<PathBuf>,
        setup: impl Fn(&mut Command) + Send + Sync + 'static,
    ) -> Self {
        Self {
            program: program.into(),
            timeout: GIT_TIMEOUT,
            setup: Arc::new(setup),
        }
    }

    /// The same git with `timeout` as the deadline of its commands, in place
    /// of `GIT_TIMEOUT`: lets a test stand a short default in for 120 s.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The same program, run with the agent environment in place of this
    /// git's setup: for checks on what a run left, which must not hand the
    /// runner's environment to anything the run planted.
    pub(crate) fn as_agent(&self, agent: &AgentEnv) -> Self {
        let agent = agent.clone();
        Self::with_setup(self.program.clone(), move |command| agent.apply(command))
    }

    /// Runs git in `dir` with `input` on its standard input (none without
    /// it), and returns what it printed, whatever its exit status.
    pub(crate) fn output<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        input: Option<&[u8]>,
    ) -> Result<Captured, GitError> {
        self.output_within(dir, args, input, self.timeout)
    }

    /// [`Git::output`] with its own deadline, still stopping the whole
    /// process tree when it passes.
    pub(crate) fn output_within<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<Captured, GitError> {
        let mut command = Command::new(&self.program);
        command.args(hardening()).args(args).current_dir(dir);
        (self.setup)(&mut command);
        // Messages in English, whatever the operator's locale.
        command.env("LC_ALL", "C").env("LANGUAGE", "");
        run_command(&mut command, input, timeout, WHOLE_OUTPUT).map_err(|error| {
            let detail = match error {
                RunError::Io(e) => format!("could not run: {e}"),
                RunError::TimedOut => {
                    format!("did not finish within {} s", timeout.as_secs())
                }
            };
            GitError::new(dir, args, detail)
        })
    }

    /// Runs git in `dir` and returns its standard output; a non-zero exit is
    /// an error carrying the end of its standard error.
    pub(crate) fn run<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<Vec<u8>, GitError> {
        self.run_within(dir, args, self.timeout)
    }

    /// [`Git::run`] with its own deadline.
    pub(crate) fn run_within<S: AsRef<OsStr>>(
        &self,
        dir: &Path,
        args: &[S],
        timeout: Duration,
    ) -> Result<Vec<u8>, GitError> {
        let output = self.output_within(dir, args, None, timeout)?;
        if output.success() {
            Ok(output.stdout)
        } else {
            Err(GitError::new(
                dir,
                args,
                format!(
                    "failed with {}: {}",
                    exit_text(output.code),
                    tail(&String::from_utf8_lossy(&output.stderr))
                ),
            ))
        }
    }
}

/// What every command of the runner's git starts with, before the
/// subcommand: `core.hooksPath` naming a folder that does not exist, so no
/// hook is found; `core.fsmonitor=false`, so no monitor command runs; and
/// `repack.updateServerInfo=false`, so that a repack, such as the one of the
/// maintenance a `fetch` starts in the background and which outlives it,
/// writes no `info/refs`: the isolation check guards that file, and a run
/// during which it changed would be quarantined for nothing (OWL-197). The
/// agent environment carries the same setting
/// (`owlshift_core::agent_env::OVERRIDES`). Given with `-c`, they win over
/// every configuration file, and reach the git commands git starts itself.
fn hardening() -> [OsString; 6] {
    [
        "-c".into(),
        hooks_nowhere().into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
        "-c".into(),
        "repack.updateServerInfo=false".into(),
    ]
}

/// `core.hooksPath=<folder>` for a folder that does not exist, named by this
/// process and the time, so nothing can have been planted there: git finds
/// no hook in it.
fn hooks_nowhere() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let base =
        std::env::temp_dir().join(format!("owlshift-no-hooks-{}-{nanos}", std::process::id()));
    let mut folder = base.clone();
    let mut n = 1;
    while fs::symlink_metadata(&folder).is_ok() {
        n += 1;
        folder = PathBuf::from(format!("{}-{n}", base.display()));
    }
    format!("core.hooksPath={}", folder.display())
}

/// The last lines of a command's standard error, enough to say why it
/// failed.
fn tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.trim().lines().collect();
    lines[lines.len().saturating_sub(3)..].join(" / ")
}

/// A git command of the executor's that failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitError {
    pub args: String,
    pub dir: PathBuf,
    pub detail: String,
}

impl GitError {
    fn new<S: AsRef<OsStr>>(dir: &Path, args: &[S], detail: String) -> Self {
        let args: Vec<_> = args
            .iter()
            .map(|arg| arg.as_ref().to_string_lossy().into_owned())
            .collect();
        Self {
            args: args.join(" "),
            dir: dir.to_owned(),
            detail,
        }
    }
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "git {} (in {}) {}",
            self.args,
            self.dir.display(),
            self.detail
        )
    }
}

impl std::error::Error for GitError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_turns_hooks_the_monitor_and_the_server_info_off() {
        let args = hardening();
        let settings: Vec<&str> = args
            .chunks(2)
            .map(|pair| {
                assert_eq!(pair[0], "-c", "{args:?}");
                pair[1].to_str().unwrap()
            })
            .collect();
        assert_eq!(settings.len(), 3, "{settings:?}");
        assert!(settings.contains(&"core.fsmonitor=false"), "{settings:?}");
        assert!(
            settings.contains(&"repack.updateServerInfo=false"),
            "{settings:?}"
        );
        let folder = settings
            .iter()
            .find_map(|setting| setting.strip_prefix("core.hooksPath="))
            .unwrap();
        assert!(fs::symlink_metadata(folder).is_err(), "{folder} exists");
    }

    /// OWL-197: git's own housekeeping, started by a command of the runner's
    /// git, writes no `info/refs`, which the isolation check guards: a `gc`
    /// stands in for the maintenance a `fetch` starts, with the same repack
    /// run as a child process. Plain git writes it. Git is hermetic: no
    /// inherited `GIT_*` variable, no system or user configuration.
    #[test]
    fn the_runners_git_leaves_the_server_info_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("gitconfig");
        fs::write(
            &global,
            "[user]\n\tname = Owlshift Test\n\temail = test@owlshift.invalid\n\
             [commit]\n\tgpgsign = false\n",
        )
        .unwrap();
        let hermetic = move |command: &mut Command| {
            for (name, _) in std::env::vars_os() {
                if name
                    .to_string_lossy()
                    .to_ascii_uppercase()
                    .starts_with("GIT_")
                {
                    command.env_remove(name);
                }
            }
            command
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", &global);
        };
        let plain = |dir: &Path, args: &[&str]| {
            let mut command = Command::new("git");
            command.args(args).current_dir(dir);
            hermetic(&mut command);
            let out = command.output().unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        let repo = tmp.path().join("repo");
        fs::create_dir(&repo).unwrap();
        plain(&repo, &["init", "--quiet"]);
        fs::write(repo.join("a.txt"), "a\n").unwrap();
        plain(&repo, &["add", "a.txt"]);
        plain(&repo, &["commit", "--quiet", "-m", "a"]);
        let refs = repo.join(".git").join("info").join("refs");

        Git::with_setup("git", hermetic.clone())
            .run(&repo, &["gc", "--quiet"])
            .unwrap();
        assert!(!refs.exists(), "the runner's git wrote {}", refs.display());

        // The control: plain git's `gc` writes it.
        plain(&repo, &["gc", "--quiet"]);
        assert!(refs.exists(), "plain git's gc wrote no {}", refs.display());
    }

    /// OWL-130: git started for the agent, outside the sandbox, runs no
    /// shell start-up file, on the CI's Linux and macOS legs alike. `ENV`, which
    /// a project may declare, is read by interactive shells only; `BASH_ENV`,
    /// which bash reads in a script, is refused as a declaration and dropped
    /// from the runner's environment (`owlshift_core::agent_env`).
    #[cfg(unix)]
    #[test]
    fn git_started_for_the_agent_runs_no_shell_start_up_file() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Stdio;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let mark = dir.join("start-up file ran");
        let startup = dir.join("startup.sh");
        fs::write(&startup, format!(": > '{}'\n", mark.display())).unwrap();
        // Git found on the agent's `PATH` can be a wrapper script (an asdf or
        // pyenv shim, Nix's `makeWrapper`).
        let wrapper = |name: &str, shebang: &str| {
            let path = dir.join(name);
            fs::write(&path, format!("#!{shebang}\nexec git \"$@\"\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let sh_wrapper = wrapper("git-sh", "/bin/sh");
        let bash_wrapper = wrapper("git-bash", "/usr/bin/env bash");

        let mut parent: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| name == "PATH" || name == "HOME")
            .collect();
        parent.push(("ENV".into(), startup.clone().into()));
        parent.push(("BASH_ENV".into(), startup.clone().into()));
        let agent = AgentEnv::for_project(parent, &["ENV"], &["ENV"])
            .unwrap()
            .without_confinement();
        assert!(agent.var("ENV").is_some());
        assert_eq!(agent.var("BASH_ENV"), None);

        for program in [&sh_wrapper, &bash_wrapper] {
            // The `&&` makes git start the alias through `sh -c`.
            let alias_ran = dir.join("alias ran");
            let alias = format!("alias.probe=!: > '{}' && true", alias_ran.display());
            Git::new(program)
                .as_agent(&agent)
                .run(dir, &["-c", alias.as_str(), "probe"])
                .unwrap();
            assert!(alias_ran.exists(), "{program:?}: the alias did not run");
            assert!(!mark.exists(), "{program:?} ran the start-up file");
            fs::remove_file(&alias_ran).unwrap();
        }

        // Controls, so that the absence of the mark above means something:
        // bash runs `BASH_ENV`'s file in a script, and this system's `sh`
        // reads `ENV`'s when interactive.
        let control = |program: &Path, args: &[&str], variable: &str| {
            let status = Command::new(program)
                .args(args)
                .env_clear()
                .env("PATH", agent.var("PATH").unwrap())
                .env(variable, &startup)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "{program:?} {args:?}: {status}");
            let ran = mark.exists();
            if ran {
                fs::remove_file(&mark).unwrap();
            }
            ran
        };
        assert!(control(&bash_wrapper, &["--version"], "BASH_ENV"));
        assert!(control(Path::new("/bin/sh"), &["-i", "-c", "true"], "ENV"));
    }
}
