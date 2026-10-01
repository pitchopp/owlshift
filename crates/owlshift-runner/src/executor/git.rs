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
//! planted there before the isolation check has seen it.

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
/// hook is found, and `core.fsmonitor=false`, so no monitor command runs.
/// Given with `-c`, they win over every configuration file.
fn hardening() -> [OsString; 4] {
    [
        "-c".into(),
        hooks_nowhere().into(),
        "-c".into(),
        "core.fsmonitor=false".into(),
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
    fn every_command_turns_hooks_and_the_monitor_off() {
        let args = hardening();
        assert_eq!(args[0], "-c");
        assert_eq!(args[2], "-c");
        assert_eq!(args[3], "core.fsmonitor=false");
        let hooks = args[1].to_str().unwrap();
        let folder = hooks.strip_prefix("core.hooksPath=").unwrap();
        assert!(fs::symlink_metadata(folder).is_err(), "{folder} exists");
    }
}
