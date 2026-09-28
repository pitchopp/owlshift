//! What the fake harness does in one run: its reply file, in TOML.
//!
//! The fake harness acts in this order: it waits (`delay_ms`); creates and
//! switches to another branch (`switch_branch`); writes files (`files`) and
//! commits them (`commit`, dated `date`); writes files in the main checkout
//! (`main_checkout`); copies a prepared `result.json`, valid or not, to the
//! brief's result path (`result`); prints `stdout` and `stderr`, then the
//! usage-limit line if `usage_limit` is set; and exits with `exit_code`, 0
//! by default or 1 with a usage limit. `switch_branch` and `main_checkout`
//! break isolation on purpose.

use std::collections::BTreeMap;
use std::path::PathBuf;

use jiff::Timestamp;
use owlshift_contracts::ids::RelativePath;
use serde::{Deserialize, Serialize};

/// The exit status of the fake harness when it cannot do what it was told:
/// an unreadable brief or reply, or a failed step. A scenario treats it as a
/// broken bench, never as a failed run.
pub const OWN_FAILURE: i32 = 2;

/// The start of the line that reports a usage limit, on standard error.
pub const USAGE_LIMIT_PREFIX: &str = "owlshift-fake-harness: usage limit reached · resets ";

/// One run of the fake harness.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    /// How long to wait before anything else, in milliseconds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub delay_ms: u64,
    /// Files to write in the worktree, by path relative to it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<RelativePath, String>,
    /// The message of a commit of those files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The commit's author and committer date. The scenario runner sets it
    /// to the step's virtual time, so commit ids are reproducible: the agent
    /// environment passes no `GIT_*` variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<Timestamp>,
    /// A branch to create and switch the worktree to, first: an isolation
    /// breach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_branch: Option<String>,
    /// Files to write in the main checkout, the one the worktree belongs to,
    /// by path relative to it: an isolation breach.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub main_checkout: BTreeMap<RelativePath, String>,
    /// A prepared result file, copied byte for byte to the brief's result
    /// path. In a scenario it is relative to the scenario's fixture folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<PathBuf>,
    /// A usage limit reached, with its reset time: no result is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_limit: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Reply {
    /// Parses a reply file.
    pub fn parse(input: &str) -> Result<Self, String> {
        let reply: Self = toml::from_str(input).map_err(|e| format!("invalid reply: {e}"))?;
        reply.validate()?;
        Ok(reply)
    }

    /// Renders a reply file.
    pub fn render(&self) -> String {
        toml::to_string(self).expect("a reply always serializes to TOML")
    }

    /// Refuses a reply that contradicts itself, or that asks for exit status
    /// 2, which is the fake harness's own failure.
    pub fn validate(&self) -> Result<(), String> {
        if self.exit_code == Some(OWN_FAILURE) {
            return Err(format!(
                "invalid reply: exit status {OWN_FAILURE} is kept for the fake harness's own failures"
            ));
        }
        if self.commit.is_some() && self.files.is_empty() {
            return Err("invalid reply: `commit` needs `files` to commit".to_owned());
        }
        if self
            .switch_branch
            .as_ref()
            .is_some_and(|branch| branch.is_empty() || branch.starts_with('-'))
        {
            return Err("invalid reply: `switch_branch` is not a branch name".to_owned());
        }
        if self.usage_limit.is_some() && self.result.is_some() {
            return Err(
                "invalid reply: a run stopped by a usage limit leaves no `result`".to_owned(),
            );
        }
        Ok(())
    }

    /// The exit status: `exit_code`, else 1 with a usage limit and 0 without.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
            .unwrap_or(if self.usage_limit.is_some() { 1 } else { 0 })
    }
}

/// The line the fake harness prints on standard error at a usage limit.
pub fn usage_limit_line(reset: Timestamp) -> String {
    format!("{USAGE_LIMIT_PREFIX}{reset}")
}

/// The reset time of the usage limit a run reported, if it reported one.
pub fn usage_limit(stderr: &str) -> Option<Timestamp> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix(USAGE_LIMIT_PREFIX)?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_is_strict_and_the_usage_limit_line_round_trips() {
        let reply = Reply::parse(
            "delay_ms = 20\nusage_limit = \"2026-09-28T15:00:00Z\"\n[files]\n\"docs/a.md\" = \"a\\n\"\n",
        )
        .unwrap();
        assert_eq!(reply.exit_code(), 1);
        assert_eq!(Reply::parse(&reply.render()).unwrap(), reply);

        for refused in [
            "delay = 20\n",
            "[files]\n\"../outside.md\" = \"x\"\n",
            "commit = \"Nothing to commit\"\n",
            "exit_code = 2\n",
            "switch_branch = \"--orphan\"\n",
            "result = \"done.json\"\nusage_limit = \"2026-09-28T15:00:00Z\"\n",
        ] {
            assert!(Reply::parse(refused).is_err(), "{refused}");
        }

        let reset = reply.usage_limit.unwrap();
        let stderr = format!("working\n{}\n", usage_limit_line(reset));
        assert_eq!(usage_limit(&stderr), Some(reset));
        assert_eq!(usage_limit("working\n"), None);
    }
}
