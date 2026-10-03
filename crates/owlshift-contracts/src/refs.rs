//! The git ref layout: claims and per-ticket state.
//!
//! `refs/owlshift/claims/<ticket>` points to a commit whose tree holds
//! [`CLAIM_FILE`]; `refs/owlshift/tickets/<ticket>` points to a commit whose
//! tree holds [`STATE_FILE`], [`QUESTIONS_FILE`] and, later, the ticket's
//! artifacts. Since OWL-122 the runner keeps the ticket ref in its dedicated
//! checkout only; pushing it to the remote comes with claims (P7).

use std::num::NonZeroU32;

use jiff::Timestamp;
use owlshift_core::decider::DeciderRule;
use owlshift_core::state::{Status, TicketState};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Stage;
use crate::brief::{ThreadEntry, validate_thread};
use crate::format::{
    self, CLAIM_FORMAT, ContractError, Format, QUESTIONS_FORMAT, TICKET_STATE_FORMAT,
};
use crate::ids::TicketId;
use crate::result::{Question, Verdict, check_verdicts};

/// The namespace of every Owlshift ref.
pub const REF_NAMESPACE: &str = "refs/owlshift/";
/// The prefix of claim refs.
pub const CLAIMS_PREFIX: &str = "refs/owlshift/claims/";
/// The prefix of ticket refs.
pub const TICKETS_PREFIX: &str = "refs/owlshift/tickets/";

/// The lease, in a claim ref's tree.
pub const CLAIM_FILE: &str = "claim.json";
/// The ticket's state, in a ticket ref's tree.
pub const STATE_FILE: &str = "state.json";
/// The plan, in a ticket ref's tree.
pub const PLAN_FILE: &str = "plan.md";
/// The step ledger, in a ticket ref's tree.
pub const LEDGER_FILE: &str = "ledger.json";
/// The questions the runner asked on the ticket, in a ticket ref's tree; the
/// answers stay on the ticket.
pub const QUESTIONS_FILE: &str = "questions.json";
/// The review findings, in a ticket ref's tree.
pub const FINDINGS_FILE: &str = "findings.json";

/// The claim ref of a ticket: `refs/owlshift/claims/<ticket>`.
pub fn claim_ref(ticket: &TicketId) -> String {
    format!("{CLAIMS_PREFIX}{ticket}")
}

/// The ticket ref of a ticket: `refs/owlshift/tickets/<ticket>`.
pub fn ticket_ref(ticket: &TicketId) -> String {
    format!("{TICKETS_PREFIX}{ticket}")
}

/// A runner's lease on a ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift claim")]
pub struct Claim {
    pub format: Format<CLAIM_FORMAT>,
    /// The machine the holding runner runs on.
    pub machine: String,
    /// The person operating that runner.
    pub operator: String,
    /// When the lease may be taken over if not renewed.
    pub expires_at: Timestamp,
}

impl Claim {
    const CONTRACT: &str = "claim";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        format::parse_json(Self::CONTRACT, CLAIM_FORMAT, input)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

/// Where a ticket stands in its pipeline.
// The stored form of the core `TicketState`, in `STATE_FILE`: convert with
// `PersistedState::from` to write and `TicketState::try_from` to read back,
// which refuses a document the machine could never be in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket state")]
pub struct PersistedState {
    pub format: Format<TICKET_STATE_FORMAT>,
    /// The stage the ticket works, returns to after its answers, or is parked at.
    pub stage: Stage,
    /// Why the ticket waits, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Waiting>,
    /// Question rounds asked so far; re-asks do not count.
    pub round: u32,
    /// Re-asks in the current round.
    pub reasks: u32,
    /// Failed runs in the current stage.
    pub failed_runs: u32,
}

/// Why a ticket waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Waiting {
    /// Questions wait for the decider.
    NeedsInput,
    /// The ticket was parked; a human restarts it.
    Parked,
    /// Parked while questions waited; a restart goes back to waiting for them.
    ParkedAwaitingInput,
}

impl PersistedState {
    const CONTRACT: &str = "ticket state";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let state: Self = format::parse_json(Self::CONTRACT, TICKET_STATE_FORMAT, input)?;
        state.validate()?;
        Ok(state)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// Checks that this is a state the ticket state machine can be in.
    pub fn validate(&self) -> Result<(), ContractError> {
        TicketState::try_from(self).map(|_| ())
    }
}

impl From<&TicketState> for PersistedState {
    fn from(state: &TicketState) -> Self {
        let waiting = match state.status() {
            Status::Active(_) => None,
            Status::NeedsInput { .. } => Some(Waiting::NeedsInput),
            Status::Parked {
                awaiting_input: false,
                ..
            } => Some(Waiting::Parked),
            Status::Parked {
                awaiting_input: true,
                ..
            } => Some(Waiting::ParkedAwaitingInput),
        };
        Self {
            format: Format,
            stage: state.stage(),
            waiting,
            round: state.round(),
            reasks: state.reasks(),
            failed_runs: state.failed_runs(),
        }
    }
}

impl TryFrom<&PersistedState> for TicketState {
    type Error = ContractError;

    fn try_from(persisted: &PersistedState) -> Result<Self, ContractError> {
        let at = persisted.stage;
        let status = match persisted.waiting {
            None => Status::Active(at),
            Some(Waiting::NeedsInput) => Status::NeedsInput { return_to: at },
            Some(Waiting::Parked) => Status::Parked {
                at,
                awaiting_input: false,
            },
            Some(Waiting::ParkedAwaitingInput) => Status::Parked {
                at,
                awaiting_input: true,
            },
        };
        TicketState::restore(
            status,
            persisted.round,
            persisted.reasks,
            persisted.failed_runs,
        )
        .map_err(|error| ContractError::invalid(PersistedState::CONTRACT, error.to_string()))
    }
}

/// The questions the runner asked on a ticket, in [`QUESTIONS_FILE`]: each
/// ask with the comment that posted it, so a brief's thread shows the ask in
/// that comment's place, and its decider, and what the last answer check
/// read; and the decisions the resolver took instead of asking.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket questions")]
pub struct TicketQuestions {
    pub format: Format<QUESTIONS_FORMAT>,
    /// Every ask the runner posted on the ticket, oldest first.
    #[serde(default)]
    pub asks: Vec<Ask>,
    /// The newest last-edit time among the decider's comments that the last
    /// answer check to give its verdicts read. Only a decider comment newer
    /// than this, and than the latest ask, is a new answer: an answer already
    /// judged is never judged again on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_through: Option<Timestamp>,
    /// The decisions the resolver took on questions of the ticket's runs,
    /// oldest first, each kept once its DECISION comment was posted (format
    /// 4), so a brief's thread shows it in that comment's place. Kept apart
    /// from the asks: a decision has no round and no decider, and must never
    /// move the latest ask that answers are timed against.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<KeptDecision>,
}

/// A decision the resolver took without the decider, as the ticket ref
/// keeps it: what the brief's `decision` entry is built from, so only the
/// runner's own record, never a comment's text, reaches a run as a decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptDecision {
    /// When the DECISION comment was posted, as the tracker recorded it.
    pub at: Timestamp,
    /// The tracker's identifier of the runner's DECISION comment.
    #[schemars(regex(pattern = r"\S"))]
    pub comment: String,
    /// The question as the raising run asked it, under that run's id.
    pub question: Question,
    #[schemars(regex(pattern = r"\S"))]
    pub decision: String,
    /// What settles it.
    #[schemars(regex(pattern = r"\S"))]
    pub basis: String,
}

impl KeptDecision {
    /// The decision as a brief's thread shows it.
    pub fn entry(&self) -> ThreadEntry {
        ThreadEntry::Decision {
            at: self.at,
            question: self.question.clone(),
            decision: self.decision.clone(),
            basis: self.basis.clone(),
        }
    }
}

/// One ask: a round's questions, or the questions of a round asked again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    pub kind: AskKind,
    pub round: NonZeroU32,
    /// When the comment was posted, as the tracker recorded it.
    pub at: Timestamp,
    /// The tracker's identifier of the runner's comment that posted it.
    #[schemars(regex(pattern = r"\S"))]
    pub comment: String,
    /// The questions, under their ids in the round.
    #[schemars(length(min = 1))]
    pub questions: Vec<Question>,
    /// Who answers it: resolved when the round was asked, and its decider
    /// for good (architecture section 4); a re-ask keeps its round's.
    pub decider: AskDecider,
    /// The verdicts of the latest answer check that gave verdicts on this
    /// ask, one per question it named, in question order (format 3): what a
    /// RESUME comment restates once the round is answered. Empty until a
    /// check gives them, and on an ask read from format 2.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verdicts: Vec<Verdict>,
}

/// The decider of an ask.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskDecider {
    /// The tracker account's identifier, compared as written.
    #[schemars(regex(pattern = r"\S"))]
    pub account: String,
    /// The rule that made them the decider.
    pub by: DeciderRule,
}

/// What an ask is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AskKind {
    /// A round of questions, numbered Q1..Qn.
    Questions,
    /// Questions of a round left open, asked again under their ids.
    Reask,
}

impl Ask {
    /// The ask as a brief's thread shows it.
    pub fn entry(&self) -> ThreadEntry {
        let (round, at, questions) = (self.round, self.at, self.questions.clone());
        match self.kind {
            AskKind::Questions => ThreadEntry::Questions {
                round,
                at,
                questions,
            },
            AskKind::Reask => ThreadEntry::Reask {
                round,
                at,
                questions,
            },
        }
    }
}

impl TicketQuestions {
    const CONTRACT: &str = "ticket questions";

    /// No ask yet.
    pub fn new() -> Self {
        Self {
            format: Format,
            asks: Vec::new(),
            checked_through: None,
            decisions: Vec::new(),
        }
    }

    /// Parses `questions.json`. A format-2 document, written before the
    /// asks kept their verdicts (OWL-123), and a format-3 one, written before
    /// the resolver's decisions were kept (OWL-138), are read as format 4
    /// with what they lack empty, and the next write gives them format 4.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let questions: Self = match Self::from_older_format(input) {
            Some(read) => read?,
            None => format::parse_json(Self::CONTRACT, QUESTIONS_FORMAT, input)?,
        };
        questions.validate()?;
        Ok(questions)
    }

    /// A format-2 or format-3 document read as format 4, or `None` for any
    /// other. Each older format is a strict subset of the next.
    fn from_older_format(input: &str) -> Option<Result<Self, ContractError>> {
        let mut document: serde_json::Value = serde_json::from_str(input).ok()?;
        let refused = match document.get("format").and_then(serde_json::Value::as_u64) {
            Some(2) => document["asks"]
                .as_array()
                .is_some_and(|asks| asks.iter().any(|ask| ask.get("verdicts").is_some()))
                .then_some("format 2 keeps no verdicts"),
            Some(3) => document
                .get("decisions")
                .is_some()
                .then_some("format 3 keeps no decisions"),
            _ => return None,
        };
        if let Some(reason) = refused {
            return Some(Err(ContractError::invalid(Self::CONTRACT, reason)));
        }
        document["format"] = QUESTIONS_FORMAT.into();
        Some(
            serde_json::from_value(document).map_err(|source| ContractError::Json {
                contract: Self::CONTRACT,
                source,
            }),
        )
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// Checks the rules the types alone do not carry: each ask names its
    /// comment and its decider's account, its verdicts keep the rules of
    /// `result.json`'s and name questions of that ask, and the asks follow a
    /// brief thread's rules (rounds increase, a round's questions are
    /// Q1..Qn, a re-ask names questions of an earlier round, in order); each
    /// decision names its comment, a decision and a basis, in time order.
    pub fn validate(&self) -> Result<(), ContractError> {
        for (n, kept) in self.decisions.iter().enumerate() {
            let missing = if kept.comment.trim().is_empty() {
                "comment"
            } else if kept.decision.trim().is_empty() {
                "decision"
            } else if kept.basis.trim().is_empty() {
                "basis"
            } else if n > 0 && self.decisions[n - 1].at > kept.at {
                return Err(ContractError::invalid(
                    Self::CONTRACT,
                    format!("decision {} is older than the one before it", n + 1),
                ));
            } else {
                continue;
            };
            return Err(ContractError::invalid(
                Self::CONTRACT,
                format!("decision {} names no {missing}", n + 1),
            ));
        }
        for ask in &self.asks {
            let missing = if ask.comment.trim().is_empty() {
                "comment"
            } else if ask.decider.account.trim().is_empty() {
                "decider"
            } else {
                continue;
            };
            return Err(ContractError::invalid(
                Self::CONTRACT,
                format!("an ask of round {} names no {missing}", ask.round),
            ));
        }
        let entries: Vec<ThreadEntry> = self.asks.iter().map(Ask::entry).collect();
        validate_thread(Self::CONTRACT, &entries)?;
        for ask in &self.asks {
            check_verdicts(Self::CONTRACT, &ask.verdicts)?;
            if let Some(verdict) = ask
                .verdicts
                .iter()
                .find(|v| !ask.questions.iter().any(|q| q.id == v.question))
            {
                return Err(ContractError::invalid(
                    Self::CONTRACT,
                    format!(
                        "an ask of round {} keeps a verdict for {}, which it did not ask",
                        ask.round, verdict.question
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The latest ask, if any.
    pub fn latest(&self) -> Option<&Ask> {
        self.asks.last()
    }

    /// The time a decider comment must be newer than to be a new answer: the
    /// latest ask's, or [`TicketQuestions::checked_through`] when that is
    /// later. `None` when nothing was asked.
    pub fn answers_after(&self) -> Option<Timestamp> {
        let asked = self.latest()?.at;
        Some(self.checked_through.map_or(asked, |read| read.max(asked)))
    }
}

impl Default for TicketQuestions {
    fn default() -> Self {
        Self::new()
    }
}
