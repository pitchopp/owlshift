//! The git ref layout: claims and per-ticket state.
//!
//! `refs/owlshift/claims/<ticket>` points to a commit whose tree holds
//! [`CLAIM_FILE`]; `refs/owlshift/tickets/<ticket>` points to a commit whose
//! tree holds [`STATE_FILE`], [`QUESTIONS_FILE`], once a delivery was made
//! [`DELIVERY_FILE`] (OWL-209), once the drain took it in or a run proposed a
//! follow-up [`INTAKE_FILE`] and [`PROPOSALS_FILE`] (OWL-245), once a design
//! or a review ran [`PLAN_FILE`] and [`REVIEW_FILE`] (OWL-300), once a write
//! followed a run [`COST_FILE`] (OWL-378), once a Design or Build run asked
//! [`SESSION_FILE`] (OWL-394), and, later, the ticket's
//! artifacts. Since OWL-122 the runner keeps the ticket ref in its dedicated
//! checkout only; pushing it to the remote comes with claims (P7).

use std::num::NonZeroU32;

use jiff::Timestamp;
use owlshift_core::decider::DeciderRule;
use owlshift_core::gate::TicketSpend;
use owlshift_core::state::{Status, TicketState};
use owlshift_core::vocab::MergeMethod;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::brief::{
    DecidedQuestion, MAX_CHOICE_BYTES, MAX_REFUSED_CHOICES, MAX_RESULT_REFUSAL_BYTES,
    ShortQuestion, ThreadEntry, validate_thread,
};
use crate::format::{
    self, CLAIM_FORMAT, COST_FORMAT, ContractError, DELIVERY_FORMAT, Format, HELD_FORMAT,
    QUESTIONS_FORMAT, SESSION_FORMAT, TICKET_STATE_FORMAT,
};
use crate::ids::{SessionId, TicketId, check_name, is_commit_id, is_lower_hex};
use crate::result::{
    self, DECISIONS_ADVICE, Decision, Followup, Question, RefusedChoice, Verdict, check_verdicts,
};
use crate::{BasisKind, Role, Stage};

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
/// The plan, in a ticket ref's tree once a design run returned one
/// (OWL-300, [`crate::review::TicketPlan`]).
pub const PLAN_FILE: &str = "plan.json";
/// The step ledger, in a ticket ref's tree.
pub const LEDGER_FILE: &str = "ledger.json";
/// The questions the runner asked on the ticket, in a ticket ref's tree; the
/// answers stay on the ticket.
pub const QUESTIONS_FILE: &str = "questions.json";
/// The pull request Owlshift delivered and follows, in a ticket ref's tree
/// (OWL-209).
pub const DELIVERY_FILE: &str = "delivery.json";
/// The intake record, in a ticket ref's tree once intake ran (OWL-245,
/// [`crate::drain::TicketIntake`]).
pub const INTAKE_FILE: &str = "intake.json";
/// The follow-ups proposed by the ticket's runs, in a ticket ref's tree once
/// one was (OWL-245, [`crate::drain::TicketProposals`]).
pub const PROPOSALS_FILE: &str = "proposals.json";
/// The review passes, their findings and the pull request reviews answered,
/// in a ticket ref's tree once a review ran (OWL-300,
/// [`crate::review::TicketReview`]).
pub const REVIEW_FILE: &str = "review.json";
/// What the ticket's runs cost, in a ticket ref's tree once a write followed
/// a run (OWL-378, [`TicketCost`]).
pub const COST_FILE: &str = "cost.json";
/// The session of the latest run that asked, in a ticket ref's tree once a
/// Design or Build run asked (OWL-394, [`TicketSession`]).
pub const SESSION_FILE: &str = "session.json";
/// What the ticket holds against the project's other tickets, in a ticket
/// ref's tree from its dispatch (OWL-403, [`TicketHeld`]).
pub const HELD_FILE: &str = "held.json";

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
    /// Fix runs pushed since the pull request was last read green; only at
    /// Watch (format 3).
    pub fix_passes: u32,
    /// Fix and rebase runs pushed since the delivery; only at Watch (format
    /// 3).
    pub repairs: u32,
    /// Review passes of the current stage (format 4, OWL-300).
    pub review_passes: u32,
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

    /// Parses a `state.json`. A format-2 one, written before the fix passes
    /// and repairs were kept (OWL-55), is read with both at 0, a format-2 or
    /// 3 one, written before the review passes were kept (OWL-300), with
    /// them at 0, and the next write gives it the current format.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let state: Self = match Self::from_older_format(input) {
            Some(read) => read?,
            None => format::parse_json(Self::CONTRACT, TICKET_STATE_FORMAT, input)?,
        };
        state.validate()?;
        Ok(state)
    }

    /// A format-2 or 3 document read as the current format, or `None` for
    /// any other. Format 2 is format 3 without the fix passes and repairs,
    /// and format 3 is format 4 without the review passes, so a field a
    /// later format added is refused in it, whatever its value.
    fn from_older_format(input: &str) -> Option<Result<Self, ContractError>> {
        let mut document: serde_json::Value = serde_json::from_str(input).ok()?;
        let fields = document.as_object_mut()?;
        let format = fields.get("format").and_then(serde_json::Value::as_u64)?;
        let lacks: &[&str] = match format {
            2 => &["fix_passes", "repairs", "review_passes"],
            3 => &["review_passes"],
            _ => return None,
        };
        for field in lacks {
            if fields.contains_key(*field) {
                return Some(Err(ContractError::invalid(
                    Self::CONTRACT,
                    format!("format {format} keeps no {field}"),
                )));
            }
            fields.insert((*field).to_owned(), 0.into());
        }
        fields.insert("format".to_owned(), TICKET_STATE_FORMAT.into());
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
            fix_passes: state.fix_passes(),
            repairs: state.repairs(),
            review_passes: state.review_passes(),
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
            persisted.fix_passes,
            persisted.repairs,
        )
        .and_then(|state| state.with_review_passes(persisted.review_passes))
        .map_err(|error| ContractError::invalid(PersistedState::CONTRACT, error.to_string()))
    }
}

/// What a ticket's runs cost, in [`COST_FILE`] (OWL-378, architecture section
/// 4, "Ticket budget"): every run the runner started for it, counted once as
/// it ended. The amounts are in millionths of a dollar, the unit the runner
/// counts in. Convert with `TicketCost::from` to write and
/// `TicketSpend::try_from` to read back, which refuses parts that contradict
/// each other.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket cost")]
pub struct TicketCost {
    pub format: Format<COST_FORMAT>,
    /// The runs counted.
    pub runs: u32,
    /// Those of them that reported no cost, each of which added nothing.
    pub unreported_runs: u32,
    /// What every run counted cost.
    pub spent_micros: u64,
    /// The spend when a person last restarted the ticket, 0 before any
    /// restart: the ticket's budget counts what was spent since.
    pub restarted_at_micros: u64,
}

impl TicketCost {
    const CONTRACT: &str = "ticket cost";

    /// Parses a `cost.json`, refusing one whose parts contradict each other.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let cost: Self = format::parse_json(Self::CONTRACT, COST_FORMAT, input)?;
        TicketSpend::try_from(&cost)?;
        Ok(cost)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

impl From<TicketSpend> for TicketCost {
    fn from(spend: TicketSpend) -> Self {
        Self {
            format: Format,
            runs: spend.runs(),
            unreported_runs: spend.unreported(),
            spent_micros: spend.spent_micros(),
            restarted_at_micros: spend.restarted_at_micros(),
        }
    }
}

impl TryFrom<&TicketCost> for TicketSpend {
    type Error = ContractError;

    fn try_from(cost: &TicketCost) -> Result<Self, ContractError> {
        TicketSpend::restore(
            cost.runs,
            cost.unreported_runs,
            cost.spent_micros,
            cost.restarted_at_micros,
        )
        .ok_or_else(|| {
            ContractError::invalid(
                TicketCost::CONTRACT,
                "more runs without a cost than runs, or a restart past the whole spend",
            )
        })
    }
}

/// The most files a [`TicketHeld`] lists: past them, or past
/// [`MAX_HELD_BYTES`], the ticket holds the whole repository (OWL-403), as a
/// pull request whose files cannot be read whole does.
pub const MAX_HELD_FILES: usize = 3_000;
/// The most bytes a rendered [`TicketHeld`] takes before its files give way
/// to the whole repository, far below the ticket ref's cap on a file.
pub const MAX_HELD_BYTES: usize = 256 * 1024;
/// The most bytes of one path a [`TicketHeld`] keeps.
pub const MAX_HELD_PATH_BYTES: usize = 4_096;
/// The widenings whose time a [`TicketHeld`] keeps: the latest ones.
pub const MAX_WIDENINGS: usize = 100;

/// What a ticket holds against the project's other tickets, in
/// [`HELD_FILE`] (OWL-403, architecture section 5, "What a ticket holds"):
/// written at its dispatch with nothing admitted, given what its Build's
/// admission takes (OWL-448, "Resources admitted at Build"), widened by the
/// files its branch changes, and never narrowed while the ticket holds
/// anything, until its pull request ends. A ticket dispatched before it was
/// kept has none and holds the whole repository. Not in `intake.json`, which
/// belongs to the ticket's text and is replaced by the next intake.
///
/// Format 1, written before OWL-448, held what its dispatch declared: it
/// reads as admitted at its dispatch, with no plan admitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket held")]
pub struct TicketHeld {
    pub format: Format<HELD_FORMAT>,
    /// The zones admitted, as written: the plan's files, each the zone of its
    /// own path, with the `zone:` labels' zones, or intake's zones with the
    /// labels' (format 1: those its dispatch declared). None, once
    /// admitted: the whole repository; with nothing admitted, none.
    #[serde(default)]
    pub zones: Vec<String>,
    /// The named resources admitted, by name: those its files match are not
    /// kept, each check reaching them from the project's list.
    #[serde(default)]
    pub resources: Vec<String>,
    /// The files its branch changed against its merge-base with the base,
    /// a rename's two paths both, each once, sorted: a file holds itself,
    /// never its folder. None while it holds the whole repository.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// When it was dispatched; none for a ticket dispatched before
    /// `held.json` was kept, which came before any other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatched_at: Option<Timestamp>,
    /// When its Build's admission took what it holds, the latest when a
    /// return to Design added a plan's files (format 2). None with a
    /// dispatch time: nothing admitted, the ticket holding nothing but the
    /// files a widening added ([`Self::admitted`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_at: Option<Timestamp>,
    /// The SHA-256 of the plan whose files its latest admission took, as 64
    /// lowercase hexadecimal digits (format 2): a plan with another hash, a
    /// return to Design's, is admitted at the next Build. None when no plan
    /// was admitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub plan_sha256: Option<String>,
    /// When widenings added files, oldest first, the latest
    /// [`MAX_WIDENINGS`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widened_at: Vec<Timestamp>,
    /// When its pull request was opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<Timestamp>,
    /// The commit, a full id, whose Verify pass passed and whose delivery a
    /// collision held back: its next step delivers that commit with no
    /// Verify pass while the branch is still there. Cleared by the delivery
    /// and by a new dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<String>,
}

impl TicketHeld {
    const CONTRACT: &str = "ticket held";

    /// What a ticket dispatched at `at` holds (OWL-448): nothing, until its
    /// Build's admission.
    pub fn dispatched(at: Timestamp) -> Self {
        Self {
            format: Format,
            zones: Vec::new(),
            resources: Vec::new(),
            files: Vec::new(),
            dispatched_at: Some(at),
            admitted_at: None,
            plan_sha256: None,
            widened_at: Vec::new(),
            delivered_at: None,
            verified: None,
        }
    }

    /// What a ticket of a ref from before P6 holds once a record must be
    /// kept for it: the whole repository, with no dispatch time, so it stays
    /// first.
    pub fn unrecorded() -> Self {
        Self {
            dispatched_at: None,
            ..Self::dispatched(Timestamp::UNIX_EPOCH)
        }
    }

    /// Whether what it lists is held: once its Build's admission took it, a
    /// record of format 1 included, or for a record with no dispatch time,
    /// a ref's from before P6. A ticket dispatched since OWL-448 and not yet
    /// admitted holds nothing but the files a widening added.
    pub fn admitted(&self) -> bool {
        self.admitted_at.is_some() || self.dispatched_at.is_none()
    }

    /// Whether it holds the whole repository: admitted, with no zone.
    pub fn whole(&self) -> bool {
        self.admitted() && self.zones.is_empty()
    }

    /// Admits `zones` (none for the whole repository) and `resources` at
    /// `at`, the plan whose files they hold being `plan_sha256` (OWL-448). A
    /// first admission takes them; a later one, a return to Design's, adds
    /// them, each once, a whole repository staying whole and listing no file.
    pub fn admit(
        &mut self,
        zones: Vec<String>,
        resources: Vec<String>,
        plan_sha256: Option<String>,
        at: Timestamp,
    ) {
        if zones.is_empty() {
            self.zones.clear();
            self.files.clear();
        } else if !self.admitted() {
            self.zones = zones;
        } else if !self.whole() {
            gather_strings(&mut self.zones, &zones);
        }
        gather_strings(&mut self.resources, &resources);
        self.admitted_at = Some(at);
        self.plan_sha256 = plan_sha256;
    }

    /// Adds the files its branch changed, at `at`, and gives those it did
    /// not hold yet, in their order; a widening that adds one keeps its
    /// time. A ticket that holds the whole repository already adds none.
    /// Past [`MAX_HELD_FILES`] or [`MAX_HELD_BYTES`], or given a path it
    /// cannot keep (empty, holding a NUL, longer than
    /// [`MAX_HELD_PATH_BYTES`]), the ticket holds the whole repository and
    /// lists no file, admitted at `at` if it was not.
    pub fn widen(&mut self, files: impl IntoIterator<Item = String>, at: Timestamp) -> Vec<String> {
        let added = self.widen_by(files, at);
        if !added.is_empty() {
            self.widened_at.push(at);
            let over = self.widened_at.len().saturating_sub(MAX_WIDENINGS);
            self.widened_at.drain(..over);
        }
        added
    }

    /// The whole repository from now, at `at`, for a widening whose files
    /// could not be read: what cannot be read is never taken as free, so a
    /// record with nothing admitted is admitted then. Whether that changed
    /// what it holds.
    pub fn widen_whole(&mut self, at: Timestamp) -> bool {
        if self.whole() {
            return false;
        }
        self.hold_whole(at);
        self.widened_at.push(at);
        let over = self.widened_at.len().saturating_sub(MAX_WIDENINGS);
        self.widened_at.drain(..over);
        true
    }

    /// The whole repository from `at`, listing no file.
    fn hold_whole(&mut self, at: Timestamp) {
        self.zones.clear();
        self.files.clear();
        if !self.admitted() {
            self.admitted_at = Some(at);
        }
    }

    /// [`Self::widen`], without keeping its time among the widenings.
    fn widen_by(&mut self, files: impl IntoIterator<Item = String>, at: Timestamp) -> Vec<String> {
        if self.whole() {
            return Vec::new();
        }
        let mut added: Vec<String> = Vec::new();
        let mut unkept = false;
        for file in files {
            if !keepable_path(&file) {
                unkept = true;
            } else if !self.files.contains(&file) && !added.contains(&file) {
                added.push(file);
            }
        }
        self.files.extend(added.iter().cloned());
        self.files.sort();
        if unkept || self.files.len() > MAX_HELD_FILES || self.render().len() > MAX_HELD_BYTES {
            self.hold_whole(at);
        }
        added
    }

    /// Parses and validates a `held.json`. A format-1 one, kept before
    /// OWL-448, is read as the current format admitted at its dispatch, with
    /// no plan admitted: it keeps holding what its dispatch declared.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let held: Self = match Self::from_older_format(input) {
            Some(read) => read?,
            None => format::parse_json(Self::CONTRACT, HELD_FORMAT, input)?,
        };
        held.validate()?;
        Ok(held)
    }

    /// A format-1 document read as the current format, or `None` for any
    /// other. Format 1 is format 2 without `admitted_at` and `plan_sha256`,
    /// which are refused in it.
    fn from_older_format(input: &str) -> Option<Result<Self, ContractError>> {
        let mut document: serde_json::Value = serde_json::from_str(input).ok()?;
        if document.get("format").and_then(serde_json::Value::as_u64) != Some(1) {
            return None;
        }
        if document.get("admitted_at").is_some() || document.get("plan_sha256").is_some() {
            return Some(Err(ContractError::invalid(
                Self::CONTRACT,
                "format 1 keeps no admission",
            )));
        }
        document["format"] = HELD_FORMAT.into();
        if let Some(at) = document.get("dispatched_at").cloned() {
            document["admitted_at"] = at;
        }
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

    /// Checks each zone keeps the rules of a `zone:` label's, each resource
    /// is a resource name, the files are none for the whole repository and
    /// otherwise paths it keeps, sorted, each once and at most
    /// [`MAX_HELD_FILES`], nothing but files kept while nothing is admitted,
    /// the plan's hash a SHA-256 in lowercase hex, the widenings at most
    /// [`MAX_WIDENINGS`], and the mark is a full commit id.
    pub fn validate(&self) -> Result<(), ContractError> {
        let invalid = |reason: String| Err(ContractError::invalid(Self::CONTRACT, reason));
        for zone in &self.zones {
            let broken = if zone.contains('*') {
                Err("holds `*`: a zone is a folder, not a pattern")
            } else {
                owlshift_core::decider::check_zone(zone)
            };
            if let Err(reason) = broken {
                return invalid(format!("the zone {zone:?} {reason}"));
            }
        }
        for name in &self.resources {
            if let Err(reason) = owlshift_core::resource::check_resource_name(name) {
                return invalid(format!("the resource {name:?} {reason}"));
            }
        }
        if self.whole() && !self.files.is_empty() {
            return invalid("lists files while it holds the whole repository".to_owned());
        }
        if !self.admitted()
            && (!self.zones.is_empty() || !self.resources.is_empty() || self.plan_sha256.is_some())
        {
            return invalid(
                "keeps zones, resources or a plan while nothing is admitted".to_owned(),
            );
        }
        if let Some(hash) = &self.plan_sha256
            && !is_lower_hex(hash, 64)
        {
            return invalid(format!(
                "plan_sha256 {hash:?} is not a SHA-256 in lowercase hex"
            ));
        }
        if self.files.len() > MAX_HELD_FILES {
            return invalid(format!(
                "{} files are listed, past the {MAX_HELD_FILES} it keeps",
                self.files.len()
            ));
        }
        if let Some(file) = self.files.iter().find(|file| !keepable_path(file)) {
            return invalid(format!("the file {file:?} is not a path it keeps"));
        }
        if self.files.windows(2).any(|pair| pair[0] >= pair[1]) {
            return invalid("the files are not sorted, each once".to_owned());
        }
        if self.widened_at.len() > MAX_WIDENINGS {
            return invalid(format!(
                "{} widenings are kept, past the {MAX_WIDENINGS} it keeps",
                self.widened_at.len()
            ));
        }
        if let Some(commit) = &self.verified
            && !is_commit_id(commit)
        {
            return invalid(format!("verified {commit:?} is not a commit id"));
        }
        Ok(())
    }
}

/// A path a [`TicketHeld`] keeps: not empty, no NUL, at most
/// [`MAX_HELD_PATH_BYTES`].
fn keepable_path(path: &str) -> bool {
    !path.is_empty() && !path.contains('\0') && path.len() <= MAX_HELD_PATH_BYTES
}

/// Adds each of `items` that `into` lacks, in order.
fn gather_strings(into: &mut Vec<String>, items: &[String]) {
    for item in items {
        if !into.contains(item) {
            into.push(item.clone());
        }
    }
}

/// The session of the latest run that asked, in [`SESSION_FILE`] (OWL-394,
/// architecture section 4, "Resuming the asking run's session"): what the
/// next run of its role at its stage needs to resume it once its questions
/// are settled, and to count its own cost. The transcript itself stays in
/// the run's directory, on the machine that ran it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket session")]
pub struct TicketSession {
    pub format: Format<SESSION_FORMAT>,
    /// The run that asked: the name of its run directory, which keeps the
    /// transcript.
    #[schemars(regex(pattern = r"^[0-9A-Za-z][0-9A-Za-z_-]{0,63}$"))]
    pub run: String,
    /// The session the run started or resumed.
    pub session: SessionId,
    /// `design` or `build`.
    pub role: Role,
    /// The stage it ran at: Design for the design role, Build for the build
    /// role.
    pub stage: Stage,
    /// What a resume must share with the run: its harness and the model it
    /// asked for, as the harness names them.
    #[schemars(regex(pattern = r"\S"))]
    pub harness: String,
    /// The ticket branch's head when the run ended.
    #[schemars(regex(pattern = "^(?:[0-9a-f]{40}|[0-9a-f]{64})$"))]
    pub head: String,
    /// When the run ended, by the runner's clock.
    pub at: Timestamp,
    /// The session's whole cost when the run ended, as its harness reported
    /// it, in millionths of a dollar: a resumed run's own cost is what it
    /// reports less this. `None` when the run reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_micros: Option<u64>,
    /// The tokens of the run's last request, its context: what a cold resume
    /// writes to the cache again. `None` when the run reported none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    /// The question round the run's questions opened, once one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<NonZeroU32>,
    /// Every question of the run is decided or answered: the next run of its
    /// role at its stage may resume the session.
    pub settled: bool,
}

impl TicketSession {
    const CONTRACT: &str = "ticket session";

    /// Parses a `session.json`, refusing one that breaks [`Self::validate`].
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let session: Self = format::parse_json(Self::CONTRACT, SESSION_FORMAT, input)?;
        session.validate()?;
        Ok(session)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// Refuses a run name that is not a plain name of a run directory, a
    /// role other than design or build, a stage that is not its role's, a
    /// blank harness, or a head that is not a full commit id.
    pub fn validate(&self) -> Result<(), ContractError> {
        let invalid = |reason: String| ContractError::invalid(Self::CONTRACT, reason);
        let plain = self.run.len() <= 64
            && self
                .run
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && self
                .run
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'));
        if !plain {
            return Err(invalid(format!(
                "run {:?} is not the name of a run directory",
                self.run
            )));
        }
        match (self.role, self.stage) {
            (Role::Design, Stage::Design) | (Role::Build, Stage::Build) => {}
            (role, stage) => {
                return Err(invalid(format!(
                    "a {} run at {stage:?} keeps no session",
                    role.as_str()
                )));
            }
        }
        check_name(&self.harness).map_err(|why| invalid(format!("harness {why}")))?;
        if !is_commit_id(&self.head) {
            return Err(invalid(format!("head {:?} is not a commit id", self.head)));
        }
        Ok(())
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
    /// The ticket's decisions document (format 15, OWL-351), once one was
    /// published: the reasoning of every kept decision, rewritten whole at
    /// each resolver run that keeps decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decisions_document: Option<DecisionsDocument>,
}

/// The ticket's decisions document, as the ticket ref keeps it (OWL-351).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DecisionsDocument {
    /// The tracker's identifier of the document, which the next resolver
    /// run updates.
    #[schemars(regex(pattern = r"\S"))]
    pub id: String,
    /// Where a person opens it, as the tracker gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
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
    /// goes to `choices`. Each at most [`MAX_RESULT_REFUSAL_BYTES`]. They
    /// hold the next runs but have no place a run can name, so an ask is
    /// not checked against them, and clears them (OWL-204, build-plan.md's
    /// "A hold kept as text").
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
        let joiner = if self.lists.is_empty() {
            LATER
        } else {
            EARLIER
        };
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
pub fn cut_to(reason: &str, max: usize) -> String {
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

/// A question as formats 2 to 11 kept it, given the shape of format 12
/// (OWL-324): each option, a string, becomes an option of that label; the
/// recommendation, a string, becomes its `why`, naming no option, since a
/// free text cannot be told to mean one. Anything else is left as it is,
/// for the strict parse to judge.
fn upgrade_question(question: &mut serde_json::Value) {
    if let Some(options) = question.get_mut("options").and_then(|o| o.as_array_mut()) {
        for option in options {
            if let Some(label) = option.as_str() {
                *option = serde_json::json!({ "label": label });
            }
        }
    }
    if let Some(recommendation) = question.get_mut("recommendation")
        && let Some(why) = recommendation.as_str()
    {
        *recommendation = serde_json::json!({ "why": why });
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
    /// The kind of that basis (format 13, OWL-343), so the delivery report
    /// lists the decisions taken on judgment first; absent from a decision
    /// kept before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis_kind: Option<BasisKind>,
    /// Its number in its DECISION comment, `Dn` (format 15, OWL-351), which
    /// holds the decisions of one resolver run: a reply naming it reverses
    /// it alone. Absent from a decision kept before, alone in its comment,
    /// which [`Self::place`] reads as 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<NonZeroU32>,
    /// The decider reversed it by replying to its DECISION comment
    /// (format 10, OWL-146): the replies a run was given and followed, up to
    /// [`Reversal::through`]. A newer reply is one no run has followed yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reversed: Option<Reversal>,
}

/// The decider's replies to a DECISION comment that a run followed: the
/// Build that delivered the ticket, or a correction run at Watch (OWL-146).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reversal {
    /// The newest last edit among those replies.
    pub through: Timestamp,
    /// The tracker's identifier of the reply edited at `through`.
    #[schemars(regex(pattern = r"\S"))]
    pub comment: String,
}

impl KeptDecision {
    /// Its number in its DECISION comment: the kept `number`, or 1 for a
    /// decision kept before OWL-351, alone in its comment.
    pub fn place(&self) -> NonZeroU32 {
        self.number.unwrap_or(NonZeroU32::MIN)
    }

    /// The decision as a brief's thread shows it, its question whole: what a
    /// thread shows of a decision the decider replied to (OWL-359).
    pub fn entry(&self) -> ThreadEntry {
        self.entry_with(DecidedQuestion::Whole(self.question.clone()))
    }

    /// The decision as a brief's thread shows it, its question by its id and
    /// text alone (OWL-359): the decision says what was chosen.
    pub fn short_entry(&self) -> ThreadEntry {
        self.entry_with(DecidedQuestion::Short(ShortQuestion {
            id: self.question.id.clone(),
            text: self.question.text.clone(),
        }))
    }

    fn entry_with(&self, question: DecidedQuestion) -> ThreadEntry {
        ThreadEntry::Decision {
            at: self.at,
            question,
            decision: self.decision.clone(),
            basis: self.basis.clone(),
            reversed: self.reversed.is_some(),
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
            decisions_document: None,
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
    /// (OWL-198), a format-9 one, written before a decision kept its
    /// reversal (OWL-146), a format-10 one, written before a question
    /// kept its `replan` (OWL-300), a format-11 one, written before a
    /// question's options and recommendation were structured (OWL-324), and
    /// a format-12 one, written before a decision kept its basis kind
    /// (OWL-343), and a format-13 one, written before it kept its asking
    /// role (OWL-345), and a format-14 one, written before it kept its
    /// number and the decisions document (OWL-351), are read as the
    /// current format with what they lack empty, a format-6 or 7
    /// Build refusal upgraded (`LegacyBuildRefusal::upgrade`), a format-8
    /// one's lists kept as text, and every question's options and
    /// recommendation upgraded (`upgrade_question`), and the next write
    /// gives them the current format. A document of format 14 to 16, which
    /// kept each decision's asking role as `asked_by` (OWL-345), is read
    /// without it, as format 17 dropped it (OWL-379).
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let questions: Self = match Self::from_older_format(input) {
            Some(read) => read?,
            None => format::parse_json(Self::CONTRACT, QUESTIONS_FORMAT, input)?,
        };
        questions.validate()?;
        Ok(questions)
    }

    /// A format-2 to 16 document read as the current format, or `None` for
    /// any other. Each older format up to 7 is a strict subset of the next,
    /// so a field a later format added is refused in it, whatever its
    /// value; format 8 reshaped Build's refusal, which formats 6 and 7 keep
    /// in their own shape, format 9 adds the typed choices to it, format 10
    /// a decision's reversal, format 11 a question's `replan`, format 12
    /// reshaped a question's options and recommendation, which every older
    /// format keeps as strings, format 13 adds a decision's basis kind and
    /// format 14 its asking role, format 15 its number and the decisions
    /// document, format 16 the runner's `recommendation` basis kind
    /// (OWL-375); format 17 drops the asking role, which formats 14 to 16
    /// are read without (OWL-379).
    fn from_older_format(input: &str) -> Option<Result<Self, ContractError>> {
        let mut document: serde_json::Value = serde_json::from_str(input).ok()?;
        let format = document.get("format").and_then(serde_json::Value::as_u64)?;
        if !(2..=16).contains(&format) {
            return None;
        }
        let kinded = format <= 12
            && document["decisions"].as_array().is_some_and(|decisions| {
                decisions
                    .iter()
                    .any(|decision| decision.get("basis_kind").is_some())
            });
        let in_a_decision = |field: &str| {
            document["decisions"]
                .as_array()
                .is_some_and(|decisions| decisions.iter().any(|d| d.get(field).is_some()))
        };
        let asker = format <= 13 && in_a_decision("asked_by");
        let numbered = format <= 14 && in_a_decision("number");
        let recommended = format <= 15
            && document["decisions"].as_array().is_some_and(|decisions| {
                decisions
                    .iter()
                    .any(|decision| decision["basis_kind"] == "recommendation")
            });
        let asked = document["asks"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|ask| ask["questions"].as_array().into_iter().flatten());
        let decided = document["decisions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|decision| &decision["question"]);
        let replan = format <= 10
            && asked
                .chain(decided)
                .any(|question| question.get("replan").is_some());
        let typed = format <= 8
            && document["build_refusal"]
                .as_object()
                .is_some_and(|kept| kept.contains_key("choices") || kept.contains_key("left_out"));
        let reversed = document["decisions"].as_array().is_some_and(|decisions| {
            decisions
                .iter()
                .any(|decision| decision.get("reversed").is_some())
        });
        let in_an_ask = |field: &str| {
            document["asks"]
                .as_array()
                .is_some_and(|asks| asks.iter().any(|ask| ask.get(field).is_some()))
        };
        let refused = if replan {
            Some("replan")
        } else if format == 2 && in_an_ask("verdicts") {
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
        } else if format <= 9 && reversed {
            Some("reversed decision")
        } else if kinded {
            Some("decision's basis kind")
        } else if asker {
            Some("decision's asking role")
        } else if numbered {
            Some("decision's number")
        } else if format <= 14 && document.get("decisions_document").is_some() {
            Some("decisions document")
        } else if recommended {
            Some("decision taken on a recommendation")
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
        // Format 12 keeps its questions' current shape.
        if format <= 11
            && let Some(asks) = document.get_mut("asks").and_then(|a| a.as_array_mut())
        {
            for ask in asks {
                if let Some(questions) = ask.get_mut("questions").and_then(|q| q.as_array_mut()) {
                    questions.iter_mut().for_each(upgrade_question);
                }
            }
        }
        if format <= 11
            && let Some(decisions) = document.get_mut("decisions").and_then(|d| d.as_array_mut())
        {
            for decision in decisions {
                if let Some(question) = decision.get_mut("question") {
                    upgrade_question(question);
                }
            }
        }
        // Formats 14 to 16 kept each decision's asking role (OWL-345);
        // format 17 does not (OWL-379).
        if let Some(decisions) = document.get_mut("decisions").and_then(|d| d.as_array_mut()) {
            for decision in decisions.iter_mut().filter_map(|d| d.as_object_mut()) {
                decision.remove("asked_by");
            }
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
            let flaw =
                kept.reason
                    .as_deref()
                    .and_then(reason_flaw)
                    .map(|flaw| format!("Build's kept refusal reason {flaw}"))
                    .or_else(|| {
                        kept.lists.iter().enumerate().find_map(|(n, list)| {
                            reason_flaw(list).map(|flaw| {
                                format!("Build's kept list of choices {} {flaw}", n + 1)
                            })
                        })
                    })
                    .or_else(|| {
                        (kept.choices.len() > MAX_REFUSED_CHOICES).then(|| {
                            format!(
                                "Build's kept refusal keeps more than {MAX_REFUSED_CHOICES} choices"
                            )
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
            } else if kept
                .reversed
                .as_ref()
                .is_some_and(|r| r.comment.trim().is_empty())
            {
                "reversing reply"
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

/// The pull request a delivery opened and Owlshift's pushes to its branch, in
/// [`DELIVERY_FILE`] (OWL-209, architecture section 5, "Watch after
/// delivery"). `owlshift watch` follows it while it has a pull request and
/// has not `ended`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket delivery")]
pub struct TicketDelivery {
    pub format: Format<DELIVERY_FORMAT>,
    /// The pull request, once the forge showed the delivery's push as its
    /// head; absent while the first push is under way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<DeliveredPullRequest>,
    /// The ticket's branch.
    pub branch: String,
    /// The branch the pull request merges into.
    pub base_branch: String,
    /// When the delivery's first push was kept under way, before its pull
    /// request could be reviewed (OWL-307): a review submitted before it is
    /// not this delivery's. Kept by a retry of the same delivery and by every
    /// later push; absent from a delivery made before OWL-307 until watch
    /// stamps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<Timestamp>,
    /// The commit Owlshift last pushed, once the forge showed it as the head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed: Option<String>,
    /// The base branch's head at that push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_at_push: Option<String>,
    /// The base commit the last rebase run whose push landed rebased onto
    /// (OWL-210): a conflict read against it is a failed run of that
    /// rebase, never a second rebase onto the same base head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebased_onto: Option<String>,
    /// A push under way, until the forge shows it as the head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushing: Option<Pushing>,
    /// The DELIVERY report of `pushed`, a fix, rebase or correction run's
    /// push that landed, while it is not on the ticket yet (OWL-218): the
    /// next pass posts it, once. Since format 2; since format 4 also the
    /// first delivery's report, and a correction's that pushed nothing
    /// (OWL-236).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owed: Option<OwedReport>,
    /// Why and when watch stopped following the pull request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<Ended>,
    /// The runner's merge on green under way (format 6, OWL-445; D14):
    /// kept just before the merge call, so a later read of the pull
    /// request merged is the runner's merge, told once, even after a crash
    /// between GitHub's answer and the ref write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merging: Option<Merging>,
    /// When a person's `owlshift continue` at Watch last asked for the
    /// merge to be tried again (format 6, OWL-445): watch tries again a head
    /// whose merge GitHub refused for good before then. Not a refusal: those
    /// live in watch's memory only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_retry: Option<Timestamp>,
}

/// The runner's merge of the pull request under way (OWL-445).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Merging {
    /// The head the merge expects: Owlshift's last push.
    pub commit: String,
    /// The project's `[forge] merge_method`.
    pub method: MergeMethod,
}

/// A delivered pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeliveredPullRequest {
    pub number: u64,
    pub url: String,
}

/// A push of Owlshift's under way.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Pushing {
    /// The commit pushed.
    pub commit: String,
    pub kind: PushKind,
    /// The base branch's head read just before the push: `base_at_push`
    /// once the push is adopted.
    pub base: String,
    /// For a rebase run's push, the base commit the run rebased onto:
    /// `rebased_onto` once the push lands or is adopted (OWL-210).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onto: Option<String>,
    /// What a fix, rebase or correction run's DELIVERY report says, kept
    /// before the push so a push a crash cut short is still reported once
    /// adopted (OWL-218). Since format 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<KeptReport>,
}

/// What a fix, rebase or correction run gave its DELIVERY report (OWL-218,
/// OWL-146), or the delivering Build run (OWL-236).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptReport {
    /// The run's `summary`.
    pub summary: String,
    /// The project gate's commands, all passed, as the run's gate ran them;
    /// absent when no gate result was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<Vec<String>>,
    /// For a fix run, the failed checks it was given.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<String>,
    /// For a correction run, the questions of the decisions the decider's
    /// replies reversed (format 3, OWL-146).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<String>,
    /// For the first delivery, the runner's own notes the report lists after
    /// the resolver's decisions, which `questions.json` keeps: the late
    /// comments a Build run was given (OWL-139). Never a resolver's decision.
    /// A newer report that replaces this one unposted lists none of them
    /// (format 4, OWL-236).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Decision>,
    /// The follow-ups the run proposed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub followups: Vec<Followup>,
    /// For a review loop's run, the pull request reviews it answers, what
    /// its report's lead line names (format 5, OWL-300).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<AnsweredReview>,
}

/// A pull request review a review loop's run answers (OWL-300).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnsweredReview {
    /// The forge's id of the review.
    pub id: u64,
    /// The forge login of its author.
    #[schemars(regex(pattern = r"\S"))]
    pub author: String,
}

/// A DELIVERY report still to be posted for the delivery's `pushed`
/// commit: a fix, rebase or correction run's push (OWL-218, OWL-146), the
/// first delivery, or a correction that pushed nothing (OWL-236). The
/// commit, and for a rebase the base it rebased onto, are the delivery's
/// `pushed` and `rebased_onto`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OwedReport {
    /// `delivery`, `fix`, `rebase` or `correction`.
    pub kind: PushKind,
    /// The fix pass or the repair the push was, from 1; for a correction,
    /// how many decisions it follows the replies to; for a review loop's
    /// run, how many reviews it answers; 1 for the first delivery.
    pub count: u32,
    /// What the run gave its report; absent for a push adopted from a ref
    /// that kept none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<KeptReport>,
    /// A correction's report whose run found the pull request already
    /// following the replies, and pushed nothing (format 4, OWL-236).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unchanged: bool,
}

/// Which run's push.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PushKind {
    Delivery,
    Fix,
    Rebase,
    /// A correction run's, after the decider reversed a decision (format 3,
    /// OWL-146).
    Correction,
    /// A review loop's run, after reviews on the pull request (format 5,
    /// OWL-300).
    Review,
}

/// Why and when watch stopped following a pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ended {
    pub why: EndedWhy,
    pub at: Timestamp,
    /// The head read, when someone else's push ended the following.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

/// Why watch stopped following.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EndedWhy {
    /// A person merged it.
    Merged,
    /// It was closed unmerged.
    Closed,
    /// A person pushed to its branch.
    HeadMoved,
}

impl TicketDelivery {
    const CONTRACT: &str = "ticket delivery";

    /// A delivery of `branch` into `base_branch` with nothing pushed yet.
    pub fn new(branch: &str, base_branch: &str) -> Self {
        Self {
            format: Format,
            pull_request: None,
            branch: branch.to_owned(),
            base_branch: base_branch.to_owned(),
            delivered_at: None,
            pushed: None,
            base_at_push: None,
            rebased_onto: None,
            pushing: None,
            owed: None,
            ended: None,
            merging: None,
            merge_retry: None,
        }
    }

    /// Parses a `delivery.json`. A format-1 one, written before the owed
    /// report and the kept report were kept (OWL-218), is read without
    /// them, a format-2 one, written before a correction run's push
    /// (OWL-146), without one, and a format-3 one, written before the first
    /// delivery's report was owed (OWL-236), without that, and a format-4
    /// one, written before a review loop's push (OWL-300), without one, and
    /// a format-5 one, written before merge on green (OWL-445), without a
    /// merge under way or a retry; the next write gives it the current
    /// format.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let delivery: Self = match Self::from_older_format(input) {
            Some(read) => read?,
            None => format::parse_json(Self::CONTRACT, DELIVERY_FORMAT, input)?,
        };
        delivery.validate()?;
        Ok(delivery)
    }

    /// A format-1 to 5 document read as the current format, or `None` for
    /// any other. Format 1 is format 2 without `owed` and `pushing.report`,
    /// format 2 is format 3 without a correction's push or report, format 3
    /// is format 4 without an owed delivery report, a report's `notes` or an
    /// owed report's `unchanged`, and format 4 is format 5 without a review
    /// loop's push, a report's `reviews` or `delivered_at` (OWL-307), and
    /// format 5 is format 6 without `merging` or `merge_retry` (OWL-445), so
    /// each is refused in an older format.
    fn from_older_format(input: &str) -> Option<Result<Self, ContractError>> {
        let mut document: serde_json::Value = serde_json::from_str(input).ok()?;
        let fields = document.as_object_mut()?;
        let format = fields.get("format").and_then(serde_json::Value::as_u64)?;
        if !(1..=5).contains(&format) {
            return None;
        }
        let pushing = fields.get("pushing").and_then(serde_json::Value::as_object);
        let owed = fields.get("owed").and_then(serde_json::Value::as_object);
        let report = pushing.is_some_and(|pushing| pushing.contains_key("report"));
        let reports = || {
            [
                pushing.and_then(|p| p.get("report")),
                owed.and_then(|owed| owed.get("report")),
            ]
            .into_iter()
            .flatten()
        };
        let correction = [
            pushing.and_then(|p| p.get("kind")),
            owed.and_then(|owed| owed.get("kind")),
        ]
        .into_iter()
        .flatten()
        .any(|kind| kind == "correction");
        let review = [
            pushing.and_then(|p| p.get("kind")),
            owed.and_then(|owed| owed.get("kind")),
        ]
        .into_iter()
        .flatten()
        .any(|kind| kind == "review");
        let reviews = reports().any(|report| report.get("reviews").is_some());
        let questions = reports().any(|report| report.get("questions").is_some());
        let notes = reports().any(|report| report.get("notes").is_some());
        let owed_delivery = owed
            .and_then(|owed| owed.get("kind"))
            .is_some_and(|kind| kind == "delivery");
        let unchanged = owed.is_some_and(|owed| owed.contains_key("unchanged"));
        let delivered_at = fields.contains_key("delivered_at");
        let merging = fields.contains_key("merging");
        let merge_retry = fields.contains_key("merge_retry");
        for (field, kept) in [
            ("owed", format == 1 && owed.is_some()),
            ("pushing.report", format == 1 && report),
            ("correction's push", format <= 2 && correction),
            ("correction's report", format <= 2 && questions),
            ("owed delivery report", format <= 3 && owed_delivery),
            ("report's notes", format <= 3 && notes),
            ("unchanged correction", format <= 3 && unchanged),
            ("review loop's push", format <= 4 && review),
            ("report's reviews", format <= 4 && reviews),
            ("delivered_at", format <= 4 && delivered_at),
            ("merging", merging),
            ("merge_retry", merge_retry),
        ] {
            if kept {
                return Some(Err(ContractError::invalid(
                    Self::CONTRACT,
                    format!("format {format} keeps no {field}"),
                )));
            }
        }
        fields.insert("format".to_owned(), DELIVERY_FORMAT.into());
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

    /// Whether watch follows it: it has a pull request and has not ended.
    pub fn followed(&self) -> bool {
        self.pull_request.is_some() && self.ended.is_none()
    }

    /// Checks the commits are full ids, the branches git branch names, the
    /// link a plain `https://` URL, that a push under way names its `onto`
    /// exactly when it is a rebase's and keeps a report only for a fix, a
    /// rebase, a correction or a review loop, that an owed report counts
    /// from 1 (exactly 1 for a delivery or a review loop), of a pushed commit
    /// with its pull request (and for a rebase its base), and is unchanged
    /// only for a correction or a review loop that kept its report (OWL-307),
    /// that a report names questions exactly when it is a correction's and
    /// keeps notes only for a delivery or a review loop, and that an ended
    /// delivery pushes nothing.
    pub fn validate(&self) -> Result<(), ContractError> {
        let invalid = |reason: String| Err(ContractError::invalid(Self::CONTRACT, reason));
        for (field, branch) in [("branch", &self.branch), ("base_branch", &self.base_branch)] {
            if !is_branch_name(branch) {
                return invalid(format!("{field} {branch:?} is not a valid branch name"));
            }
        }
        let commits = [
            ("pushed", self.pushed.as_deref()),
            ("base_at_push", self.base_at_push.as_deref()),
            ("rebased_onto", self.rebased_onto.as_deref()),
            (
                "pushing.commit",
                self.pushing.as_ref().map(|p| p.commit.as_str()),
            ),
            (
                "pushing.base",
                self.pushing.as_ref().map(|p| p.base.as_str()),
            ),
            (
                "pushing.onto",
                self.pushing.as_ref().and_then(|p| p.onto.as_deref()),
            ),
            (
                "ended.head",
                self.ended.as_ref().and_then(|e| e.head.as_deref()),
            ),
            (
                "merging.commit",
                self.merging.as_ref().map(|m| m.commit.as_str()),
            ),
        ];
        for (field, commit) in commits {
            if let Some(commit) = commit
                && !is_commit_id(commit)
            {
                return invalid(format!("{field} {commit:?} is not a full commit id"));
            }
        }
        if let Some(pr) = &self.pull_request {
            let rest = pr.url.strip_prefix("https://").unwrap_or_default();
            if rest.is_empty() || rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return invalid(format!("pull_request.url {:?} is not an https URL", pr.url));
            }
            if pr.number == 0 {
                return invalid("pull_request.number is 0".to_owned());
            }
        }
        if let Some(pushing) = &self.pushing
            && (pushing.kind == PushKind::Rebase) != pushing.onto.is_some()
        {
            return invalid(
                "a push under way names its onto if and only if it is a rebase's".to_owned(),
            );
        }
        if let Some(pushing) = &self.pushing
            && pushing.kind == PushKind::Delivery
            && pushing.report.is_some()
        {
            return invalid("a delivery's push keeps no run's report".to_owned());
        }
        // A correction's report names the questions it follows the replies
        // to, and no other report does (OWL-146).
        let reports = [
            self.pushing
                .as_ref()
                .and_then(|p| p.report.as_ref().map(|r| (p.kind, r))),
            self.owed
                .as_ref()
                .and_then(|o| o.report.as_ref().map(|r| (o.kind, r))),
        ];
        if reports
            .into_iter()
            .flatten()
            .any(|(kind, report)| (kind == PushKind::Correction) == report.questions.is_empty())
        {
            return invalid(
                "a report names questions if and only if it is a correction's".to_owned(),
            );
        }
        // The runner's notes are those of a Build's delivery: the first one
        // (OWL-236), or a review loop's (OWL-307).
        if reports.into_iter().flatten().any(|(kind, report)| {
            !matches!(kind, PushKind::Delivery | PushKind::Review) && !report.notes.is_empty()
        }) {
            return invalid(
                "a report keeps notes only if it is a delivery's or a review loop's".to_owned(),
            );
        }
        // A review loop's report names the reviews it answers, each once,
        // and no other report does (OWL-300).
        for (kind, report) in reports.into_iter().flatten() {
            if (kind == PushKind::Review) == report.reviews.is_empty() {
                return invalid(
                    "a report names reviews if and only if it is a review loop's".to_owned(),
                );
            }
            if let Some(review) = report.reviews.iter().enumerate().find_map(|(n, r)| {
                (check_name(&r.author).is_err()
                    || report.reviews[..n].iter().any(|seen| seen.id == r.id))
                .then_some(r)
            }) {
                return invalid(format!(
                    "a report names review {} twice, or without its author",
                    review.id
                ));
            }
        }
        if let Some(owed) = &self.owed {
            // A correction, or a review loop's Build (OWL-307), that changed
            // nothing.
            if owed.unchanged && !matches!(owed.kind, PushKind::Correction | PushKind::Review) {
                return invalid(
                    "an owed report is unchanged only if it is a correction's or a review loop's"
                        .to_owned(),
                );
            }
            // No push is ever adopted for it, so its run always kept its
            // report, which names its questions or its reviews (the rules
            // above).
            if owed.unchanged && owed.report.is_none() {
                return invalid("an unchanged owed report keeps its run's report".to_owned());
            }
            if owed.count == 0 {
                return invalid("an owed report counts from 1".to_owned());
            }
            if matches!(owed.kind, PushKind::Delivery | PushKind::Review) && owed.count != 1 {
                return invalid("a delivery's or a review loop's owed report counts 1".to_owned());
            }
            if self.pushed.is_none() || self.pull_request.is_none() {
                return invalid(
                    "an owed report needs the pushed commit and its pull request".to_owned(),
                );
            }
            if owed.kind == PushKind::Rebase && self.rebased_onto.is_none() {
                return invalid("an owed rebase report needs rebased_onto".to_owned());
            }
        }
        if self.ended.is_some() && self.pushing.is_some() {
            return invalid("an ended delivery has a push under way".to_owned());
        }
        if self.ended.is_some() && self.pull_request.is_none() {
            return invalid("a delivery ended with no pull request".to_owned());
        }
        Ok(())
    }
}

/// Git's rules for a branch name (`git check-ref-format --branch`), the
/// forge adapter's `Branch` too: safe in a refspec and a lease.
fn is_branch_name(name: &str) -> bool {
    let bad_component = name
        .split('/')
        .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"));
    let bad_char = name.chars().any(|c| {
        c.is_ascii_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    });
    !(name.is_empty()
        || name.starts_with('-')
        || name.ends_with('.')
        || name == "@"
        || name.contains("..")
        || name.contains("@{")
        || bad_component
        || bad_char)
}
