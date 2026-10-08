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
use crate::brief::{
    MAX_CHOICE_BYTES, MAX_REFUSED_CHOICES, MAX_RESULT_REFUSAL_BYTES, ThreadEntry, validate_thread,
};
use crate::format::{
    self, CLAIM_FORMAT, ContractError, Format, QUESTIONS_FORMAT, TICKET_STATE_FORMAT,
};
use crate::ids::TicketId;
use crate::result::{
    self, DECISIONS_ADVICE, Question, RefusedChoice, Verdict, check_verdicts,
};

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
/// result was refused, and every list of choices refused since no Build
/// result asked, until a Build result is accepted; and why the
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
    /// result is accepted (format 6, OWL-192), and every refused list of
    /// choices until a Build result asks (format 8, OWL-195), typed since
    /// format 9 (OWL-198): what the next Build run is told, whatever
    /// command runs it. Kept apart from the asks: a Build refusal belongs
    /// to no ask, and a new ask must not clear it.
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

/// A Build run's refused results, as the ticket ref keeps them for the next
/// Build run (OWL-192): every refused choice of a hold, oldest first, typed
/// (format 9, OWL-198), and the latest refusal when it listed none.
///
/// A refusal that lists choices adds them and drops `reason`, which holds
/// no choice; any other refusal told to the next run sets `reason` and
/// keeps the choices ([`BuildRefusal::keep_choices`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildRefusal {
    /// Why the latest refused Build result was refused, when it listed no
    /// choices (a broken shape, a refused `done`): the runner's message, at
    /// most [`MAX_RESULT_REFUSAL_BYTES`]. Absent when the latest refusal
    /// listed choices; a later list drops it, since it holds no choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"\S"))]
    pub reason: Option<String>,
    /// The reason of every refusal that listed the build role's own
    /// decisions, as formats 6 to 8 kept it (OWL-195), oldest first: each
    /// quotes as text the choices it refused, none of which was asked, so
    /// all are still open. Read from those formats only: a later list
    /// goes to `choices`. Each at most [`MAX_RESULT_REFUSAL_BYTES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(inner(regex(pattern = r"\S")))]
    pub lists: Vec<String>,
    /// Every choice the refusals that listed the build role's own
    /// decisions listed since `lists` and no Build result asked, oldest
    /// first, at most [`MAX_REFUSED_CHOICES`] (format 9, OWL-198): what the
    /// next Build brief's `refused_choices` tells. A choice's place here,
    /// from 1, is the same for every run the hold tells.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_REFUSED_CHOICES))]
    pub choices: Vec<RefusedChoice>,
    /// The choices listed past [`MAX_REFUSED_CHOICES`], counted, not kept;
    /// only once `choices` is full.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub left_out: u32,
}

/// Whether a count is zero, for serde.
fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// What joins the items a held Build run is told, OWL-192's words.
const LATER: &str = "\n\nA later result was refused too: ";
/// What joins the choices kept as text by formats 6 to 8 to the typed
/// choices' advice (OWL-198).
const EARLIER: &str = "\n\nEarlier results were refused too, their choices quoted here and \
                       not in `refused_choices`: ";
/// The bytes each item a Build run is told keeps at least, when the items
/// do not fit whole in [`MAX_RESULT_REFUSAL_BYTES`].
const TOLD_FLOOR: usize = 160;

impl BuildRefusal {
    /// Whether a refusal of the build role's own decisions holds the next
    /// Build run to asking them (OWL-186): its brief's `decisions_refused`.
    pub fn holds(&self) -> bool {
        !self.lists.is_empty() || !self.choices.is_empty()
    }

    /// Keeps the choices of a refusal that listed them (OWL-198): added
    /// after the kept ones, as listed, up to [`MAX_REFUSED_CHOICES`], the
    /// rest counted in `left_out`; the reason, which held no choice, goes.
    pub fn keep_choices(&mut self, listed: &[RefusedChoice]) {
        let room = MAX_REFUSED_CHOICES.saturating_sub(self.choices.len());
        self.choices.extend(listed.iter().take(room).cloned());
        let past = u32::try_from(listed.len().saturating_sub(room)).unwrap_or(u32::MAX);
        self.left_out = self.left_out.saturating_add(past);
        self.reason = None;
    }

    /// What the next Build brief's `result_refusal` says, within
    /// [`MAX_RESULT_REFUSAL_BYTES`]. With typed choices kept (OWL-198): the
    /// advice of a refusal of decisions, which says the choices are in
    /// `refused_choices` and counts those left out, told whole; then the
    /// lists formats 6 to 8 kept as text and the latest reason, sharing the
    /// rest of the room. Without: every list, oldest first, then the latest
    /// reason, sharing the room (OWL-195). Items that do not fit whole share
    /// the room: those shorter than their share stay whole, the others are
    /// cut to it, their beginning kept and the cut marked. Past the number
    /// of items that fit at 160 bytes each, the first item and the newest
    /// ones are told, and a note counts the lists left out between them.
    pub fn told(&self) -> String {
        let items: Vec<String> = self.lists.iter().chain(&self.reason).cloned().collect();
        if self.choices.is_empty() {
            return share(items, MAX_RESULT_REFUSAL_BYTES);
        }
        let mut told = ContractError::invalid(
            result::CONTRACT,
            format!(
                "{DECISIONS_ADVICE}. The choices listed by every result refused for them since \
                 no Build run asked are in `refused_choices`, oldest first"
            ),
        )
        .to_string();
        if self.left_out > 0 {
            told.push_str(&format!(
                ". {} more choices they listed are not kept, past the {MAX_REFUSED_CHOICES} \
                 there: ask too about every other choice of what the work delivers that the \
                 branch's commits took without the decider",
                self.left_out
            ));
        }
        if items.is_empty() {
            return told;
        }
        let joiner = if self.lists.is_empty() { LATER } else { EARLIER };
        told.push_str(joiner);
        let room = MAX_RESULT_REFUSAL_BYTES.saturating_sub(told.len());
        told.push_str(&share(items, room));
        told
    }
}

/// `items`, joined after "A later result was refused too:", within
/// `budget` bytes (OWL-195): items that do not fit whole share the room,
/// and past the number that fit at [`TOLD_FLOOR`] bytes each, the first
/// item and the newest ones are told, a note counting the lists left out
/// between them.
fn share(mut items: Vec<String>, budget: usize) -> String {
    let fits = |n: usize| n * TOLD_FLOOR + n.saturating_sub(1) * LATER.len() <= budget;
    let mut note = None;
    if !fits(items.len()) {
        let mut newest = items.len() - 1;
        while newest > 0 && !fits(newest + 2) {
            newest -= 1;
        }
        let left_out = items.len() - 1 - newest;
        let kept = items.split_off(items.len() - newest);
        items.truncate(1);
        items.push(format!(
            "[{left_out} more refused lists of choices are left out here for lack of room]"
        ));
        note = Some(1);
        items.extend(kept);
    }
    let room = budget.saturating_sub(items.len().saturating_sub(1) * LATER.len());
    let mut caps = vec![0; items.len()];
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| items[i].len());
    let mut left = room;
    for (rank, &i) in order.iter().enumerate() {
        caps[i] = items[i].len().min(left / (items.len() - rank));
        left -= caps[i];
    }
    let mut told = String::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            told.push_str(if note == Some(i) { "\n\n" } else { LATER });
        }
        told.push_str(&cut_to(item, caps[i]));
    }
    told
}

/// `reason` cut to [`MAX_RESULT_REFUSAL_BYTES`] on a character boundary,
/// its beginning kept and the cut marked.
pub fn cut_reason(reason: String) -> String {
    if reason.len() <= MAX_RESULT_REFUSAL_BYTES {
        return reason;
    }
    cut_to(&reason, MAX_RESULT_REFUSAL_BYTES)
}

/// `reason` cut to at most `max` bytes on a character boundary, its
/// beginning kept and the cut marked when there is room for the mark.
pub(crate) fn cut_to(reason: &str, max: usize) -> String {
    const CUT: &str = " [cut]";
    if reason.len() <= max {
        return reason.to_owned();
    }
    let mut end = max.saturating_sub(CUT.len());
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    if max < CUT.len() {
        return reason[..end].to_owned();
    }
    format!("{}{CUT}", &reason[..end])
}

/// Build's kept refusal as formats 6 and 7 wrote it (OWL-192, OWL-194),
/// read with its own types before it becomes a [`BuildRefusal`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyBuildRefusal {
    reason: String,
    decisions: bool,
    #[serde(default)]
    decisions_reason: Option<String>,
}

impl LegacyBuildRefusal {
    /// The refusal as format 8 kept it, and format 9 reads it: the
    /// refusal of decisions that started the hold is the first list; a
    /// latest reason after it, of a kind the older format did not keep, is
    /// kept as a list too, so no later refusal drops it (it is told where
    /// it was); a hold without a held reason was started by its latest
    /// one, a list. `None` for a held reason without the hold, which the
    /// older format refused.
    fn upgrade(self) -> Option<BuildRefusal> {
        Some(match (self.decisions_reason, self.decisions) {
            (Some(_), false) => return None,
            (Some(held), true) => BuildRefusal {
                lists: vec![held, self.reason],
                ..BuildRefusal::default()
            },
            (None, true) => BuildRefusal {
                lists: vec![self.reason],
                ..BuildRefusal::default()
            },
            (None, false) => BuildRefusal {
                reason: Some(self.reason),
                ..BuildRefusal::default()
            },
        })
    }
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
    /// written before Build's refusal was kept (OWL-192), a format-6 one,
    /// written before the resolver's refusal was kept (OWL-191), and a
    /// format-7 one, written before every refused list was kept (OWL-195),
    /// and a format-8 one, written before the refused choices were typed
    /// (OWL-198), are read as the current format with what they lack empty,
    /// a format-6 or 7 Build refusal upgraded (`LegacyBuildRefusal::upgrade`)
    /// and a format-8 one's lists kept as text, and the next write gives
    /// them the current format.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let questions: Self = match Self::from_older_format(input) {
            Some(read) => read?,
            None => format::parse_json(Self::CONTRACT, QUESTIONS_FORMAT, input)?,
        };
        questions.validate()?;
        Ok(questions)
    }

    /// A format-2 to 8 document read as the current format, or `None` for
    /// any other. Each older format up to 7 is a strict subset of the next,
    /// so a field a later format added is refused in it, whatever its
    /// value; format 8 reshaped Build's refusal, which formats 6 and 7 keep
    /// in their own shape, and format 9 adds the typed choices to it.
    fn from_older_format(input: &str) -> Option<Result<Self, ContractError>> {
        let mut document: serde_json::Value = serde_json::from_str(input).ok()?;
        let format = document.get("format").and_then(serde_json::Value::as_u64)?;
        if !(2..=8).contains(&format) {
            return None;
        }
        let typed = document["build_refusal"]
            .as_object()
            .is_some_and(|kept| kept.contains_key("choices") || kept.contains_key("left_out"));
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
        } else if format <= 6 && document.get("resolver_refusal").is_some() {
            Some("resolver refusal")
        } else if typed {
            Some("typed refused choices")
        } else {
            None
        };
        if let Some(field) = refused {
            return Some(Err(ContractError::invalid(
                Self::CONTRACT,
                format!("format {format} keeps no {field}"),
            )));
        }
        if format <= 7
            && let Some(kept) = document
                .get_mut("build_refusal")
                .filter(|kept| !kept.is_null())
        {
            let legacy: LegacyBuildRefusal = match serde_json::from_value(kept.take()) {
                Ok(legacy) => legacy,
                Err(source) => {
                    return Some(Err(ContractError::Json {
                        contract: Self::CONTRACT,
                        source,
                    }));
                }
            };
            let Some(upgraded) = legacy.upgrade() else {
                return Some(Err(ContractError::invalid(
                    Self::CONTRACT,
                    "Build's kept refusal keeps a refusal of decisions without `decisions`",
                )));
            };
            *kept = serde_json::to_value(upgraded).expect("a Build refusal serializes");
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
    /// order; Build's kept refusal keeps the same bounds on its reason and
    /// on each of its lists, at most [`MAX_REFUSED_CHOICES`] choices, each
    /// text of at most [`MAX_CHOICE_BYTES`], counts choices left out only
    /// once full, and keeps a reason or a choice; the resolver's keeps the
    /// same bounds.
    pub fn validate(&self) -> Result<(), ContractError> {
        if let Some(flaw) = self.resolver_refusal.as_deref().and_then(reason_flaw) {
            return Err(ContractError::invalid(
                Self::CONTRACT,
                format!("the resolver's kept refusal reason {flaw}"),
            ));
        }
        if let Some(kept) = &self.build_refusal {
            let flaw = kept
                .reason
                .as_deref()
                .and_then(reason_flaw)
                .map(|flaw| format!("Build's kept refusal reason {flaw}"))
                .or_else(|| {
                    kept.lists.iter().enumerate().find_map(|(n, list)| {
                        reason_flaw(list)
                            .map(|flaw| format!("Build's kept list of choices {} {flaw}", n + 1))
                    })
                })
                .or_else(|| {
                    (kept.choices.len() > MAX_REFUSED_CHOICES).then(|| {
                        format!("Build's kept refusal keeps more than {MAX_REFUSED_CHOICES} choices")
                    })
                })
                .or_else(|| {
                    kept.choices.iter().position(|c| {
                        c.question.len() > MAX_CHOICE_BYTES || c.recorded.len() > MAX_CHOICE_BYTES
                    })
                    .map(|n| {
                        format!(
                            "Build's kept choice {} has a text over {MAX_CHOICE_BYTES} bytes",
                            n + 1
                        )
                    })
                })
                .or_else(|| {
                    (kept.left_out > 0 && kept.choices.len() < MAX_REFUSED_CHOICES).then(|| {
                        "Build's kept refusal counts choices left out with room for them"
                            .to_owned()
                    })
                })
                .or_else(|| {
                    (kept.reason.is_none() && !kept.holds()).then(|| {
                        "Build's kept refusal keeps neither a reason nor a list of choices"
                            .to_owned()
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
