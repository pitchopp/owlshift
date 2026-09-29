//! The runner's own git: the commands the executor runs for itself (the
//! worktree, the isolation check, the checks around the gate), never for an
//! agent. The gate's checks run with the agent environment
//! ([`Git::as_agent`]).
//!
//! Each command runs through `owlshift_platform::process::run_command`, as
//! the root of a process tree stopped at [`GIT_TIMEOUT`]. Its output is kept
//! whole ([`WHOLE_OUTPUT`]): the probes' 64 KiB would cut a large
//! repository's status and blind the isolation check.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

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
            setup: Arc::new(setup),
        }
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
        let mut command = Command::new(&self.program);
        command.args(args).current_dir(dir);
        (self.setup)(&mut command);
        // Messages in English, whatever the operator's locale.
        command.env("LC_ALL", "C").env("LANGUAGE", "");
        run_command(&mut command, input, GIT_TIMEOUT, WHOLE_OUTPUT).map_err(|error| {
            let detail = match error {
                RunError::Io(e) => format!("could not run: {e}"),
                RunError::TimedOut => {
                    format!("did not finish within {} s", GIT_TIMEOUT.as_secs())
                }
            };
            GitError::new(dir, args, detail)
        })
    }

    /// Runs git in `dir` and returns its standard output; a non-zero exit is
    /// an error carrying the end of its standard error.
    pub(crate) fn run<S: AsRef<OsStr>>(&self, dir: &Path, args: &[S]) -> Result<Vec<u8>, GitError> {
        let output = self.output(dir, args, None)?;
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
