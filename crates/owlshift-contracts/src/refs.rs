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
use crate::brief::{MAX_RESULT_REFUSAL_BYTES, ThreadEntry, validate_thread};
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
/// read, and why the last check's result was refused, when it was; the
/// decisions the resolver took instead of asking; why Build's latest
/// result was refused, until a Build result is accepted; and why the
/// resolver's latest result was refused, until one is accepted or a round
/// is kept.
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
    /// Why the runner refused the latest Build result, kept until a Build
    /// result is accepted (format 6, OWL-192): what the next Build run is
    /// told, whatever command runs it. Kept apart from the asks: a Build
    /// refusal belongs to no ask, and a new ask must not clear it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_refusal: Option<BuildRefusal>,
    /// Why the runner refused the latest resolver result (format 7,
    /// OWL-191): what the next resolver run is told, whatever command runs
    /// it. Its questions went to the decider in a fallback round; it is kept
    /// for the case where that round's post or keeping failed, and a later
    /// command's Build asks them again. A later refusal replaces it; an
    /// accepted resolver result, or any round kept, clears it; at most
    /// [`MAX_RESULT_REFUSAL_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"\S"))]
    pub resolver_refusal: Option<String>,
}

/// A Build run's refused result, as the ticket ref keeps it for the next
/// Build run (OWL-192).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildRefusal {
    /// Why the latest refused Build result was refused: the runner's
    /// message, at most [`MAX_RESULT_REFUSAL_BYTES`].
    #[schemars(regex(pattern = r"\S"))]
    pub reason: String,
    /// Whether a refusal of the build role's own decisions holds the next
    /// Build run to asking them (OWL-186): set by such a refusal, kept
    /// through every later refusal, and cleared only once a Build result
    /// asks.
    pub decisions: bool,
    /// The refusal of decisions that holds the run, when a later refusal
    /// took its place as `reason`: it quotes the choices still open, which
    /// the next run is told first. Only with `decisions`; at most
    /// [`MAX_RESULT_REFUSAL_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"\S"))]
    pub decisions_reason: Option<String>,
}

impl BuildRefusal {
    /// What the next Build brief's `result_refusal` says: the refusal of
    /// decisions that holds the run, then the latest one, when they differ,
    /// cut to [`MAX_RESULT_REFUSAL_BYTES`] with its beginning kept.
    pub fn told(&self) -> String {
        match &self.decisions_reason {
            None => self.reason.clone(),
            Some(held) => cut_reason(format!(
                "{held}\n\nA later result was refused too: {}",
                self.reason
            )),
        }
    }
}

/// `reason` cut to [`MAX_RESULT_REFUSAL_BYTES`] on a character boundary,
/// its beginning kept and the cut marked.
pub fn cut_reason(reason: String) -> String {
    if reason.len() <= MAX_RESULT_REFUSAL_BYTES {
        return reason;
    }
    const CUT: &str = " [cut]";
    let mut end = MAX_RESULT_REFUSAL_BYTES - CUT.len();
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{CUT}", &reason[..end])
}

/// Why a kept refusal reason is refused: blank, or past the cap.
fn reason_flaw(reason: &str) -> Option<&'static str> {
    if reason.trim().is_empty() {
        Some("is blank")
    } else if reason.len() > MAX_RESULT_REFUSAL_BYTES {
        Some("is over the cap")
    } else {
        None
    }
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
    /// Why the runner refused the `result.json` of the latest answer check
    /// on this ask, or a file its `artifacts` name, when it did (format 5,
    /// OWL-184): what the next check on it is told, in its brief's
    /// `result_refusal`, whatever command runs it. Any other outcome of a
    /// kept check clears it; at most [`MAX_RESULT_REFUSAL_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_refusal: Option<String>,
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
            build_refusal: None,
            resolver_refusal: None,
        }
    }

    /// Parses `questions.json`. A format-2 document, written before the
    /// asks kept their verdicts (OWL-123), a format-3 one, written before
    /// the resolver's decisions were kept (OWL-138), a format-4 one, written
    /// before an ask kept its check's refusal (OWL-184), a format-5 one,
    /// written before Build's refusal was kept (OWL-192), and a format-6
    /// one, written before the resolver's refusal was kept (OWL-191), are
    /// read as the current format with what they lack empty, and the next
    /// write gives them the current format.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let questions: Self = match Self::from_older_format(input) {
            Some(read) => read?,
            None => format::parse_json(Self::CONTRACT, QUESTIONS_FORMAT, input)?,
        };
        questions.validate()?;
        Ok(questions)
    }

    /// A format-2 to 6 document read as the current format, or `None` for
    /// any other. Each older format is a strict subset of the next, so a
    /// field a later format added is refused in it, whatever its value.
    fn from_older_format(input: &str) -> Option<Result<Self, ContractError>> {
        let mut document: serde_json::Value = serde_json::from_str(input).ok()?;
        let format = document.get("format").and_then(serde_json::Value::as_u64)?;
        if !(2..=6).contains(&format) {
            return None;
        }
        let in_an_ask = |field: &str| {
            document["asks"]
                .as_array()
                .is_some_and(|asks| asks.iter().any(|ask| ask.get(field).is_some()))
        };
        let refused = if format == 2 && in_an_ask("verdicts") {
            Some("verdicts")
        } else if format <= 3 && document.get("decisions").is_some() {
            Some("decisions")
        } else if format <= 4 && in_an_ask("result_refusal") {
            Some("refusal")
        } else if format <= 5 && document.get("build_refusal").is_some() {
            Some("Build refusal")
        } else if document.get("resolver_refusal").is_some() {
            Some("resolver refusal")
        } else {
            None
        };
        if let Some(field) = refused {
            return Some(Err(ContractError::invalid(
                Self::CONTRACT,
                format!("format {format} keeps no {field}"),
            )));
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
    /// Q1..Qn, a re-ask names questions of an earlier round, in order), and a
    /// kept refusal is not blank and at most [`MAX_RESULT_REFUSAL_BYTES`];
    /// each decision names its comment, a decision and a basis, in time
    /// order; Build's kept refusal and the resolver's keep the same bounds,
    /// and a held refusal of decisions only with `decisions`.
    pub fn validate(&self) -> Result<(), ContractError> {
        if let Some(flaw) = self.resolver_refusal.as_deref().and_then(reason_flaw) {
            return Err(ContractError::invalid(
                Self::CONTRACT,
                format!("the resolver's kept refusal reason {flaw}"),
            ));
        }
        if let Some(kept) = &self.build_refusal {
            let flaw = reason_flaw(&kept.reason)
                .map(|flaw| format!("Build's kept refusal reason {flaw}"))
                .or_else(|| {
                    let held = kept.decisions_reason.as_deref()?;
                    Some(match reason_flaw(held) {
                        Some(flaw) => format!("Build's kept refusal of decisions {flaw}"),
                        None if !kept.decisions => {
                            "Build's kept refusal keeps a refusal of decisions without \
                             `decisions`"
                                .to_owned()
                        }
                        None => return None,
                    })
                });
            if let Some(flaw) = flaw {
                return Err(ContractError::invalid(Self::CONTRACT, flaw));
            }
        }
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
            } else if ask
                .result_refusal
                .as_ref()
                .is_some_and(|r| r.trim().is_empty())
            {
                "refusal reason"
            } else if ask
                .result_refusal
                .as_ref()
                .is_some_and(|r| r.len() > MAX_RESULT_REFUSAL_BYTES)
            {
                return Err(ContractError::invalid(
                    Self::CONTRACT,
                    format!(
                        "an ask of round {} keeps a refusal reason over \
                         {MAX_RESULT_REFUSAL_BYTES} bytes",
                        ask.round
                    ),
                ));
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

    /// Keeps what an answer check that gave its verdicts read and found: the
    /// answers it read through are judged, so only a newer comment of the
    /// decider is a new answer, and its verdicts go with the latest ask, the
    /// one they judge, for the round's RESUME. `read_through` is the newest
    /// last edit among the decider's comments the check read.
    pub fn keep_check(&mut self, read_through: Option<Timestamp>, verdicts: &[Verdict]) {
        self.checked_through = read_through.max(self.checked_through);
        if let Some(ask) = self.asks.last_mut() {
            ask.verdicts = verdicts.to_vec();
        }
    }

    /// Keeps why the latest answer check's result was refused, on the latest
    /// ask, for the next check on it; `None` clears it (OWL-184).
    pub fn keep_refusal(&mut self, reason: Option<String>) {
        if let Some(ask) = self.asks.last_mut() {
            ask.result_refusal = reason;
        }
    }

    /// The asks of `round` in order, its questions and a re-ask's, each with
    /// the verdicts kept on it: what a RESUME restates.
    pub fn round_asks(
        &self,
        round: NonZeroU32,
    ) -> impl Iterator<Item = (&[Question], &[Verdict])> + '_ {
        self.asks
            .iter()
            .filter(move |ask| ask.round == round)
            .map(|ask| (&ask.questions[..], &ask.verdicts[..]))
    }
}

impl Default for TicketQuestions {
    fn default() -> Self {
        Self::new()
    }
}
