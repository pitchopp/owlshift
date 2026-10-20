//! A ticket's step in a child process (OWL-399, architecture section 5,
//! "Parallel", "The step, in a child process"): the envelope `owlshift
//! watch` hands the child `owlshift continue`, [`StepEnvelope`]; the record
//! of how the step ended, [`StepOutcome`], which the child writes last in
//! the ticket's folder; and what a step carries to the next,
//! [`TicketCarry`] in `carry.json` beside it. None of them is in the ticket
//! ref: they are the runner's own, on one machine.

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use owlshift_core::gate::{Chain, ChainCost};

use crate::format::{
    self, CARRY_FORMAT, ContractError, Format, STEP_ENVELOPE_FORMAT, STEP_OUTCOME_FORMAT,
};
use crate::ids::TicketId;
use crate::{Role, Stage};

/// The longest attempt id, in bytes.
pub const MAX_ATTEMPT: usize = 80;

/// What watch decided a step of a ticket is, so that the child never applies
/// a stale decision: the kind of step, the ticket ref watch read, and the
/// generation of the carry it read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift step envelope")]
pub struct StepEnvelope {
    pub format: Format<STEP_ENVELOPE_FORMAT>,
    pub ticket: TicketId,
    /// This attempt's id, unique to it: the name of its outcome record.
    /// ASCII letters, digits and `-`, at most [`MAX_ATTEMPT`] bytes.
    #[schemars(regex(pattern = r"^[A-Za-z0-9-]{1,80}$"))]
    pub attempt: String,
    pub step: StepKind,
    /// The ticket ref's commit watch read; none for an intake of a ticket
    /// that has no ref yet.
    #[serde(default)]
    pub commit: Option<String>,
    /// What the ref kept but its runs' cost when watch read it, as watch
    /// compares it: the hold the child computes keys on it.
    pub progress: String,
    /// The decider's newest edit watch decided on, which lifts a hold.
    #[serde(default)]
    pub answer: Option<Timestamp>,
    /// The generation of the carry watch read at `commit`, 0 for none.
    #[serde(default)]
    pub generation: u64,
}

/// The kind of a step, with what each kind needs that watch alone knows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepKind {
    /// The drain's intake.
    Intake,
    /// An answer check and its RESUME.
    Answer {
        /// The ticket whose turn the project's slot is, which the RESUME
        /// names, when another ticket holds it.
        #[serde(default)]
        slot_held_by: Option<TicketId>,
    },
    /// The drain's dispatch, its first stage step.
    Dispatch,
    /// A resumed stage step, while the tracker shows `shown`.
    Resume { shown: Shown, stage: Stage },
    /// A fix or a rebase at Watch.
    Repair {
        /// How long ago watch first read a failure on the head, in
        /// milliseconds.
        #[serde(default)]
        failing_for_ms: Option<u64>,
        #[serde(default)]
        kept: KeptFollowing,
    },
    /// A correction at Watch.
    Correct {
        #[serde(default)]
        kept: KeptFollowing,
    },
    /// A review loop's start at Watch.
    Review,
}

/// The visible stage watch read before a resume.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Shown {
    Working,
    NeedsInput,
}

/// What watch keeps of a ticket's pull request that a repair or a
/// correction updates (OWL-219, OWL-232), and the forge's pause: handed to
/// the child and merged back from its outcome.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptFollowing {
    /// The stage last recorded as moved out of Owlshift's states.
    #[serde(default)]
    pub moved: Option<String>,
    /// The owed report's refusal last warned of.
    #[serde(default)]
    pub unposted: Option<KeptUnposted>,
    /// No forge read before then: a rate-limited answer.
    #[serde(default)]
    pub paused_until: Option<Timestamp>,
}

/// The commit of an owed report and the refusal warned of.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptUnposted {
    pub commit: String,
    pub reason: String,
}

impl StepEnvelope {
    const CONTRACT: &str = "step envelope";

    /// Parses an envelope, refusing an attempt id that is not a file name.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let envelope: Self = format::parse_json(Self::CONTRACT, STEP_ENVELOPE_FORMAT, input)?;
        check_attempt(Self::CONTRACT, &envelope.attempt)?;
        Ok(envelope)
    }

    /// One line of JSON, for a command's argument.
    pub fn render(&self) -> String {
        serde_json::to_string(self).expect("contract types always serialize to JSON")
    }
}

fn check_attempt(contract: &'static str, attempt: &str) -> Result<(), ContractError> {
    let ok = !attempt.is_empty()
        && attempt.len() <= MAX_ATTEMPT
        && attempt.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(ContractError::invalid(
            contract,
            "the attempt id holds other than ASCII letters, digits and `-`, or is empty or too long",
        ))
    }
}

/// How a step ended, as the child records it in `outcome-<attempt>.json`,
/// written last.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift step outcome")]
pub struct StepOutcome {
    pub format: Format<STEP_OUTCOME_FORMAT>,
    pub ticket: TicketId,
    #[schemars(regex(pattern = r"^[A-Za-z0-9-]{1,80}$"))]
    pub attempt: String,
    /// The ticket ref watch read, as its envelope said.
    #[serde(default)]
    pub commit: Option<String>,
    pub progress: String,
    #[serde(default)]
    pub answer: Option<Timestamp>,
    pub end: StepEnd,
    /// The ticket ref after the step.
    pub after: RefAfter,
    /// The hold the step calls for, by watch's rule, none when it calls for
    /// none.
    #[serde(default)]
    pub hold: Option<StepHold>,
    /// What watch keeps of the ticket's pull request, as the step left it.
    #[serde(default)]
    pub kept: KeptFollowing,
    /// When the step ended.
    pub at: Timestamp,
}

/// How a step ended, as far as watch tells ends apart.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "end", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepEnd {
    /// The ticket ref, or the stage watch read, moved since: nothing ran.
    Stale,
    /// The open pull requests reached their cap: nothing ran.
    Capped { open_prs: u32, max_open_prs: u32 },
    /// The admission gesture is not an admitter's: nothing ran.
    NotAdmitted { account: String, reason: String },
    /// Another command held the project: nothing ran.
    Busy,
    /// The harness reached its usage limit.
    UsageLimit {
        #[serde(default)]
        resets_at: Option<Timestamp>,
    },
    /// A comment that makes a person the blocker landed, or a park whose
    /// comment did not, and the ticket's state was not kept.
    NotKept { landed: LandedKind },
    /// One stage step kept its state: the ticket is at `stage`.
    Stepped { stage: Stage },
    /// Anything else: a delivery, a refusal, a park, a round, a look.
    Other,
}

/// What reached the ticket before its state could not be kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LandedKind {
    Resume,
    Questions,
    PremiseFalse,
    Reask,
    Reply,
    Parked,
    Intake,
    ParkedUnposted,
    Quarantined,
}

/// The ticket ref after a step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "ref", rename_all = "snake_case", deny_unknown_fields)]
pub enum RefAfter {
    /// It is at `commit`, keeping `progress`.
    Present { commit: String, progress: String },
    /// There is none.
    Absent,
    /// It could not be read.
    Unread,
}

/// A hold a step calls for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "hold", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepHold {
    /// Every step waits until the usage limit resets.
    Paused { until: Timestamp },
    /// The ticket is held until `until`, or until a person acts when none,
    /// unless its decider edits a comment or its ref makes progress.
    Held {
        #[serde(default)]
        until: Option<Timestamp>,
    },
}

impl StepOutcome {
    const CONTRACT: &str = "step outcome";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let outcome: Self = format::parse_json(Self::CONTRACT, STEP_OUTCOME_FORMAT, input)?;
        check_attempt(Self::CONTRACT, &outcome.attempt)?;
        Ok(outcome)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

/// What a ticket's last stage step left for its next one
/// (architecture section 5, "What a step carries"), kept in `carry.json`
/// with the ticket-ref commit that step left and a generation raised at
/// each write.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket carry")]
pub struct TicketCarry {
    pub format: Format<CARRY_FORMAT>,
    pub commit: String,
    pub generation: u64,
    pub chain: KeptChain,
    /// A Build run of the delivery already integrated late comments
    /// (OWL-139).
    pub integrated: bool,
}

/// The resolver's chain as kept: the runs in a row it settled, what its
/// resolver runs and its asking runs cost (none once one reported no cost),
/// and the asking roles that took recommendations at its bound (OWL-375).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptChain {
    pub settled: u32,
    #[serde(default)]
    pub resolver_spent_micros: Option<u64>,
    #[serde(default)]
    pub asking_spent_micros: Option<u64>,
    #[serde(default)]
    pub recommended: Vec<Role>,
}

impl From<Chain> for KeptChain {
    fn from(chain: Chain) -> Self {
        let kept = |cost| match cost {
            ChainCost::Known(micros) => Some(micros),
            ChainCost::Unreported => None,
        };
        Self {
            settled: chain.settled(),
            resolver_spent_micros: kept(chain.cost()),
            asking_spent_micros: kept(chain.asking_cost()),
            recommended: chain.recommended(),
        }
    }
}

impl KeptChain {
    /// The chain it keeps.
    pub fn chain(&self) -> Chain {
        let cost = |micros: Option<u64>| micros.map_or(ChainCost::Unreported, ChainCost::Known);
        Chain::restore(
            self.settled,
            cost(self.resolver_spent_micros),
            cost(self.asking_spent_micros),
            self.recommended.iter().copied(),
        )
    }
}

impl TicketCarry {
    const CONTRACT: &str = "ticket carry";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        format::parse_json(Self::CONTRACT, CARRY_FORMAT, input)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(step: StepKind) -> StepEnvelope {
        StepEnvelope {
            format: Format,
            ticket: TicketId::new("OWL-1").unwrap(),
            attempt: "1792324800000-4242-1".to_owned(),
            step,
            commit: Some("a".repeat(40)),
            progress: "p".to_owned(),
            answer: Some("2026-10-20T09:00:00Z".parse().unwrap()),
            generation: 3,
        }
    }

    #[test]
    fn every_envelope_reads_back_as_written() {
        let kept = KeptFollowing {
            moved: Some("Backlog".to_owned()),
            unposted: Some(KeptUnposted {
                commit: "b".repeat(40),
                reason: "the tracker is down".to_owned(),
            }),
            paused_until: Some("2026-10-20T10:00:00Z".parse().unwrap()),
        };
        for step in [
            StepKind::Intake,
            StepKind::Answer {
                slot_held_by: Some(TicketId::new("OWL-2").unwrap()),
            },
            StepKind::Dispatch,
            StepKind::Resume {
                shown: Shown::NeedsInput,
                stage: Stage::Intake,
            },
            StepKind::Repair {
                failing_for_ms: Some(90_000),
                kept: kept.clone(),
            },
            StepKind::Correct { kept: kept.clone() },
            StepKind::Review,
        ] {
            let envelope = envelope(step);
            let text = envelope.render();
            assert!(!text.contains('\n'), "{text}");
            assert_eq!(StepEnvelope::parse(&text).unwrap(), envelope);
        }
    }

    #[test]
    fn an_attempt_that_is_no_file_name_or_a_newer_format_is_refused() {
        for attempt in ["", "../x", "a b", &"a".repeat(MAX_ATTEMPT + 1)] {
            let mut bad = envelope(StepKind::Intake);
            bad.attempt = attempt.to_owned();
            let error = StepEnvelope::parse(&bad.render()).unwrap_err();
            assert!(error.to_string().contains("attempt id"), "{error}");
        }
        let newer = envelope(StepKind::Intake)
            .render()
            .replacen("\"format\":1", "\"format\":2", 1);
        assert!(matches!(
            StepEnvelope::parse(&newer),
            Err(ContractError::NewerFormat { .. })
        ));
    }

    #[test]
    fn an_outcome_and_a_carry_read_back_as_written() {
        let outcome = StepOutcome {
            format: Format,
            ticket: TicketId::new("OWL-1").unwrap(),
            attempt: "1-2-3".to_owned(),
            commit: None,
            progress: "p".to_owned(),
            answer: None,
            end: StepEnd::NotKept {
                landed: LandedKind::Resume,
            },
            after: RefAfter::Present {
                commit: "c".repeat(40),
                progress: "q".to_owned(),
            },
            hold: Some(StepHold::Held { until: None }),
            kept: KeptFollowing::default(),
            at: "2026-10-20T09:00:00Z".parse().unwrap(),
        };
        assert_eq!(StepOutcome::parse(&outcome.render()).unwrap(), outcome);

        let mut chain = Chain::default();
        chain.settled_once();
        chain.resolved(None);
        chain.took_recommendations(Role::Build);
        let carry = TicketCarry {
            format: Format,
            commit: "d".repeat(40),
            generation: 2,
            chain: KeptChain::from(chain),
            integrated: true,
        };
        let read = TicketCarry::parse(&carry.render()).unwrap();
        assert_eq!(read, carry);
        assert_eq!(read.chain.chain(), chain);
    }
}
