//! A broken base (OWL-418, architecture section 5, "Parallel", "A broken
//! base"): the project's checks of its bases, `base.json` in the project's
//! folder ([`ProjectBase`]), written under the project's lock and replaced
//! whole; and a ticket's failure waiting for its base, `base_wait.json` in
//! the ticket's folder ([`BaseWait`]), with a done Build's result for its
//! gate to be run again. What they mean is the core's
//! ([`owlshift_core::base`]); these are their files, the runner's own on one
//! machine, never in a ticket ref.

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use owlshift_core::base::{Check, Checks, Entry, Failing, Failure, Hold, Step, Verdict};

use crate::format::{self, BASE_FORMAT, BASE_WAIT_FORMAT, ContractError, Format};
use crate::result::{Decision, Followup, RunResult};

/// A project's checks of its bases.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift project base checks")]
pub struct ProjectBase {
    pub format: Format<BASE_FORMAT>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<KeptCheck>,
}

/// What is known of one commit and its commands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptCheck {
    /// The commit checked, a full id.
    pub commit: String,
    /// The runner's key of the setup and gate commands checked.
    pub commands: String,
    pub entry: KeptEntry,
}

/// A check due, or one that ran.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum KeptEntry {
    /// A ticket's failure waits for a check that could not run yet.
    Pending {
        failing: KeptFailing,
        since: Timestamp,
    },
    /// A check ran, the default branch's head being `head`.
    Checked {
        head: String,
        checked: Timestamp,
        verdict: KeptVerdict,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hold: Option<KeptHold>,
    },
}

/// A command a gate run failed on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptFailing {
    pub step: KeptStep,
    pub command: String,
}

/// The part of a gate run a command belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum KeptStep {
    Setup,
    Gate,
}

/// What a check found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "verdict", rename_all = "snake_case", deny_unknown_fields)]
pub enum KeptVerdict {
    Passed,
    /// The command it failed on, the reason, and the end of its output.
    Failed {
        step: KeptStep,
        command: String,
        reason: String,
        tail: String,
    },
}

/// A hold on the commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptHold {
    pub since: Timestamp,
    /// The operator was notified of it.
    #[serde(default)]
    pub told: bool,
}

impl ProjectBase {
    const CONTRACT: &str = "project base checks";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        format::parse_json(Self::CONTRACT, BASE_FORMAT, input)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// The file holding `checks`.
    pub fn from_checks(checks: &Checks) -> Self {
        Self {
            format: Format,
            checks: checks
                .checks
                .iter()
                .map(|check| KeptCheck {
                    commit: check.commit.clone(),
                    commands: check.commands.clone(),
                    entry: match &check.entry {
                        Entry::Pending { failing, since_ms } => KeptEntry::Pending {
                            failing: KeptFailing::from(failing),
                            since: at(*since_ms),
                        },
                        Entry::Checked {
                            head,
                            checked_ms,
                            verdict,
                            hold,
                        } => KeptEntry::Checked {
                            head: head.clone(),
                            checked: at(*checked_ms),
                            verdict: match verdict {
                                Verdict::Passed => KeptVerdict::Passed,
                                Verdict::Failed(failure) => KeptVerdict::Failed {
                                    step: failure.failing.step.into(),
                                    command: failure.failing.command.clone(),
                                    reason: failure.reason.clone(),
                                    tail: failure.tail.clone(),
                                },
                            },
                            hold: hold.map(|hold| KeptHold {
                                since: at(hold.since_ms),
                                told: hold.told,
                            }),
                        },
                    },
                })
                .collect(),
        }
    }

    /// The checks it holds.
    pub fn checks(&self) -> Checks {
        Checks {
            checks: self
                .checks
                .iter()
                .map(|kept| Check {
                    commit: kept.commit.clone(),
                    commands: kept.commands.clone(),
                    entry: match &kept.entry {
                        KeptEntry::Pending { failing, since } => Entry::Pending {
                            failing: failing.failing(),
                            since_ms: since.as_millisecond(),
                        },
                        KeptEntry::Checked {
                            head,
                            checked,
                            verdict,
                            hold,
                        } => Entry::Checked {
                            head: head.clone(),
                            checked_ms: checked.as_millisecond(),
                            verdict: match verdict {
                                KeptVerdict::Passed => Verdict::Passed,
                                KeptVerdict::Failed {
                                    step,
                                    command,
                                    reason,
                                    tail,
                                } => Verdict::Failed(Failure {
                                    failing: Failing {
                                        step: (*step).into(),
                                        command: command.clone(),
                                    },
                                    reason: reason.clone(),
                                    tail: tail.clone(),
                                }),
                            },
                            hold: hold.map(|hold| Hold {
                                since_ms: hold.since.as_millisecond(),
                                told: hold.told,
                            }),
                        },
                    },
                })
                .collect(),
        }
    }
}

impl KeptFailing {
    /// The core's failing command.
    pub fn failing(&self) -> Failing {
        Failing {
            step: self.step.into(),
            command: self.command.clone(),
        }
    }
}

impl From<&Failing> for KeptFailing {
    fn from(failing: &Failing) -> Self {
        Self {
            step: failing.step.into(),
            command: failing.command.clone(),
        }
    }
}

impl From<Step> for KeptStep {
    fn from(step: Step) -> Self {
        match step {
            Step::Setup => Self::Setup,
            Step::Gate => Self::Gate,
        }
    }
}

impl From<KeptStep> for Step {
    fn from(step: KeptStep) -> Self {
        match step {
            KeptStep::Setup => Self::Setup,
            KeptStep::Gate => Self::Gate,
        }
    }
}

/// A ticket's gate or setup failure waiting for its base, valid while the
/// ticket is at Build in `round`, configured at `configured_at` with
/// `commands`, on the branch head `head`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift base wait")]
pub struct BaseWait {
    pub format: Format<BASE_WAIT_FORMAT>,
    pub configured_at: String,
    pub commands: String,
    pub round: u32,
    /// The branch's head the failure was found on, a full commit id.
    pub head: String,
    pub failing: KeptFailing,
    /// A done Build's, whose gate alone runs again once the base passes;
    /// none for a failure before the harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<KeptDone>,
}

/// What a done Build left for its gate's next run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptDone {
    /// Its result, accepted when the run ended.
    pub result: RunResult,
    /// The runner's notes gathered for the delivery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Decision>,
    /// The follow-ups gathered for the delivery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub followups: Vec<Followup>,
    /// The newest edit of the decider's comments its brief held: a comment
    /// edited later is one it did not see (OWL-139).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<Timestamp>,
    /// It was the run given late comments to integrate, which happens once
    /// per delivery (OWL-139).
    #[serde(default)]
    pub integrating: bool,
}

impl BaseWait {
    const CONTRACT: &str = "base wait";

    /// Parses a wait; one whose result was written by an Owlshift of
    /// another result format does not parse, and its Build runs again.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        format::parse_json(Self::CONTRACT, BASE_WAIT_FORMAT, input)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

/// The instant `ms` milliseconds after the epoch, held to the range a
/// timestamp has.
fn at(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(if ms < 0 {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_survive_their_file() {
        let install = Failing {
            step: Step::Setup,
            command: "sh ci/owlshift/setup.sh".to_owned(),
        };
        let mut checks = Checks::default();
        checks.mark_pending("a", "k", &install, 1_000);
        checks.record_check(
            ("b", "k", "h"),
            2_000,
            Verdict::Failed(Failure {
                failing: install.clone(),
                reason: "setup: exit status 1".to_owned(),
                tail: "no tarball".to_owned(),
            }),
            Some(&install),
        );
        checks.claim_told("b", "k");
        checks.record_check(("c", "k", "h"), 3_000, Verdict::Passed, None);
        let text = ProjectBase::from_checks(&checks).render();
        assert_eq!(ProjectBase::parse(&text).unwrap().checks(), checks);
        assert!(text.contains("\"state\": \"pending\""), "{text}");
        assert!(text.contains("\"verdict\": \"failed\""), "{text}");
    }

    #[test]
    fn a_newer_format_is_refused() {
        assert!(ProjectBase::parse(r#"{"format":2}"#).is_err());
        assert!(BaseWait::parse(r#"{"format":2}"#).is_err());
    }
}
