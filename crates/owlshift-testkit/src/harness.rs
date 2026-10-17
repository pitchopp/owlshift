//! The fake harness as the executor drives it: `owlshift-fake-harness`
//! launched with the brief the executor wrote and a reply file.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::executor::harness::drive_plain;
use owlshift_runner::executor::{
    Harness, HarnessEnd, HarnessError, HarnessRun, HarnessStatus, RunLog, SandboxNeeds,
};

use owlshift_adapters::harness::Usage;

use crate::reply::{cost_usd, usage_limit};

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
}

impl Harness for FakeHarness {
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let mut command = Command::new(&self.program);
        command
            .arg("--brief")
            .arg(run.brief_file)
            .arg("--reply")
            .arg(&self.reply);
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

    /// The reply's folder and the bench's own files, read.
    fn sandbox_needs(&self, _agent: &AgentEnv) -> Result<SandboxNeeds, HarnessError> {
        let mut readable = self.readable.clone();
        readable.extend(self.reply.parent().map(Path::to_path_buf));
        Ok(SandboxNeeds {
            readable,
            ..SandboxNeeds::default()
        })
    }
}
