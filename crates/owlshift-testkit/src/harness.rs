//! The fake harness as the executor drives it: `owlshift-fake-harness`
//! launched with the brief the executor wrote and a reply file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use owlshift_contracts::ids::SessionId;
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::executor::harness::drive_plain;
use owlshift_runner::executor::{
    Harness, HarnessEnd, HarnessError, HarnessRun, HarnessStatus, RunLog, RunSession, SandboxNeeds,
};

use owlshift_adapters::harness::Usage;

use crate::reply::{FAKE_SESSION_ENV, cost_usd, transcript_place, usage_limit};

/// `owlshift-fake-harness`, playing one reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeHarness {
    /// The `owlshift-fake-harness` program.
    pub program: PathBuf,
    /// The reply file it plays: outside the run directory, which the sandbox
    /// hides.
    pub reply: PathBuf,
    /// What else it reads when confined, such as the files the bench's git
    /// configuration names in its home.
    pub readable: Vec<PathBuf>,
    /// It keeps sessions as Claude Code does (OWL-394): in the folder
    /// [`FAKE_SESSION_ENV`] names, each run of a session reporting the whole
    /// session's cost, and a resume of one it does not hold refused.
    pub sessions: bool,
}

impl Harness for FakeHarness {
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let mut command = Command::new(&self.program);
        command
            .arg("--brief")
            .arg(run.brief_file)
            .arg("--reply")
            .arg(&self.reply);
        match run.session {
            Some(RunSession::Start(id)) => command.args(["--session-id", id.as_str()]),
            Some(RunSession::Resume(id)) => command.args(["--resume", id.as_str()]),
            None => &mut command,
        };
        Ok(command)
    }

    /// A usage-limit line on standard error is a usage limit, exit status 0
    /// is a completed run, anything else a failure. A cost line on standard
    /// error is the run's reported cost (OWL-337); without one, the run
    /// reports no usage.
    fn drive(
        &self,
        _run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        let run = drive_plain(child, log)?;
        let status = match (
            usage_limit(&String::from_utf8_lossy(&run.stderr)),
            run.exit_code,
        ) {
            (Some(reset), _) => HarnessStatus::UsageLimit {
                resets_at: Some(reset),
            },
            (None, Some(0)) => HarnessStatus::Completed,
            (None, Some(code)) => HarnessStatus::Failed(format!("exit status {code}")),
            (None, None) => HarnessStatus::Failed("ended by a signal".to_owned()),
        };
        let mut end = HarnessEnd::new(run.exit_code, status);
        end.usage = cost_usd(&String::from_utf8_lossy(&run.stderr)).map(|cost| Usage {
            cost_usd: Some(cost),
            ..Usage::default()
        });
        Ok(end)
    }

    /// The reply's folder and the bench's own files, read; with sessions,
    /// the folder they live in.
    fn sandbox_needs(&self, _agent: &AgentEnv) -> Result<SandboxNeeds, HarnessError> {
        let mut readable = self.readable.clone();
        readable.extend(self.reply.parent().map(Path::to_path_buf));
        Ok(SandboxNeeds {
            readable,
            session_variable: self.sessions.then_some(FAKE_SESSION_ENV),
            ..SandboxNeeds::default()
        })
    }

    fn session_key(&self) -> Option<String> {
        self.sessions.then(|| "fake".to_owned())
    }

    /// [`transcript_place`] in `folder`, a regular file reached through no
    /// link.
    fn session_transcript(&self, folder: &Path, id: &SessionId) -> io::Result<Option<String>> {
        let place = transcript_place(id.as_str());
        let regular = |path: &Path| fs::symlink_metadata(path).map(|meta| meta.file_type()).ok();
        let found = regular(&folder.join("sessions")).is_some_and(|kind| kind.is_dir())
            && regular(&folder.join(&place)).is_some_and(|kind| kind.is_file());
        Ok(found.then_some(place))
    }
}
