//! The fake harness as the executor drives it: `owlshift-fake-harness`
//! launched with the brief the executor wrote and a reply file.

use std::io;
use std::path::PathBuf;
use std::process::{Child, Command};

use owlshift_runner::executor::harness::drive_plain;
use owlshift_runner::executor::{
    Harness, HarnessEnd, HarnessError, HarnessRun, HarnessStatus, RunLog,
};

use crate::reply::usage_limit;

/// `owlshift-fake-harness`, playing one reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeHarness {
    /// The `owlshift-fake-harness` program.
    pub program: PathBuf,
    /// The reply file it plays.
    pub reply: PathBuf,
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
    /// is a completed run, anything else a failure.
    fn drive(
        &self,
        _run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        let run = drive_plain(child, log)?;
        let status = match (usage_limit(&String::from_utf8_lossy(&run.stderr)), run.exit_code) {
            (Some(reset), _) => HarnessStatus::UsageLimit {
                resets_at: Some(reset),
            },
            (None, Some(0)) => HarnessStatus::Completed,
            (None, Some(code)) => HarnessStatus::Failed(format!("exit status {code}")),
            (None, None) => HarnessStatus::Failed("ended by a signal".to_owned()),
        };
        Ok(HarnessEnd::new(run.exit_code, status))
    }
}
