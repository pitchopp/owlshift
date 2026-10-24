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
    self, CARRY_FORMAT, ContractError, Format, OBLIGATION_FORMAT, OUTSIDE_FORMAT, RUN_FORMAT,
    STEP_ENVELOPE_FORMAT, STEP_OUTCOME_FORMAT,
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
    /// What watch keeps of the ticket's pull request and the forge's pause,
    /// which the step may change and its outcome gives back.
    #[serde(default)]
    pub kept: KeptFollowing,
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
    },
    /// A correction at Watch.
    Correct,
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
/// correction updates (OWL-219, OWL-232), and the forge's pause, which a
/// read of the reviews may set: handed to the child and merged back from
/// its outcome.
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
        && attempt
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
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
    /// Another command held the project or the ticket: nothing ran.
    Busy,
    /// Every run slot of the machine was taken (OWL-409): nothing ran, and
    /// watch tries again at a later pass. Format 2.
    NoSlot,
    /// The harness reached its usage limit.
    UsageLimit {
        #[serde(default)]
        resets_at: Option<Timestamp>,
    },
    /// A Build held for its base (OWL-418): `commit`, the commit it is
    /// configured at, fails `command` in a fresh worktree too, or its check
    /// is `pending`, waiting for the other steps in flight. Format 4.
    BaseHeld {
        commit: String,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        pending: bool,
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

/// A hold a step calls for. A usage limit holds its harness machine-wide
/// (`<data>/usage/`, OWL-404), never every step of a watch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "hold", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepHold {
    /// The ticket is held until `until`, or until a person acts when none,
    /// unless its decider edits a comment or its ref makes progress.
    Held {
        #[serde(default)]
        until: Option<Timestamp>,
    },
}

impl StepOutcome {
    const CONTRACT: &str = "step outcome";

    /// Parses an outcome record. A format-1, 2 or 3 one, which a watch that
    /// ran before OWL-409, OWL-404 or OWL-418 may have left for its
    /// successor, is read as the current format: format 2 only adds the
    /// `no_slot` end, refused in format 1, format 3 only drops the `paused`
    /// hold, read as none, since the machine's usage state now holds a
    /// harness at its limit, and format 4 only adds the `base_held` end,
    /// refused before.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let outcome: Self = match Self::from_older_format(input) {
            Some(read) => read?,
            None => format::parse_json(Self::CONTRACT, STEP_OUTCOME_FORMAT, input)?,
        };
        check_attempt(Self::CONTRACT, &outcome.attempt)?;
        Ok(outcome)
    }

    /// A format-1, 2 or 3 record read as the current format, or `None` for
    /// any other.
    fn from_older_format(input: &str) -> Option<Result<Self, ContractError>> {
        let mut document: serde_json::Value = serde_json::from_str(input).ok()?;
        let format = document.get("format").and_then(serde_json::Value::as_u64)?;
        if !matches!(format, 1..=3) {
            return None;
        }
        if document
            .pointer("/end/end")
            .and_then(serde_json::Value::as_str)
            == Some("base_held")
        {
            return Some(Err(ContractError::invalid(
                Self::CONTRACT,
                "formats before 4 have no `base_held` end",
            )));
        }
        if format == 1
            && document
                .pointer("/end/end")
                .and_then(serde_json::Value::as_str)
                == Some("no_slot")
        {
            return Some(Err(ContractError::invalid(
                Self::CONTRACT,
                "format 1 has no `no_slot` end",
            )));
        }
        if document
            .pointer("/hold/hold")
            .and_then(serde_json::Value::as_str)
            == Some("paused")
            && let Some(record) = document.as_object_mut()
        {
            record.remove("hold");
        }
        document["format"] = STEP_OUTCOME_FORMAT.into();
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

/// What the runner knows of a ticket's branch and ref between its runs
/// (OWL-400; architecture section 8, "Isolation among steps", "The run
/// file"), kept in `run.json` in the ticket's folder, written only under
/// the ticket's lock: what a sibling's isolation check holds the ticket to
/// once it ended, and what the ticket's next step must find.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket run")]
pub struct TicketRun {
    pub format: Format<RUN_FORMAT>,
    pub ticket: TicketId,
    /// Raised at every write of the file, so that a sibling's check sees
    /// that something happened under the ticket's lock since its snapshot.
    pub generation: u64,
    /// The commit of the ticket's branch its last check verified, or the
    /// runner last moved it to; none while the branch does not exist.
    #[serde(default)]
    pub tip: Option<String>,
    /// The commit the runner last wrote as the ticket's ref; none while it
    /// has none.
    #[serde(default)]
    pub ticket_ref: Option<String>,
    /// The name of the ticket worktree's entry in `worktrees/` of the common
    /// git folder, once a run recorded it.
    #[serde(default)]
    pub entry: Option<String>,
    /// A write of the ref, or a move of the branch, under way.
    #[serde(default, skip_serializing_if = "RunWriting::is_empty")]
    pub writing: RunWriting,
}

/// The writes under way of a [`TicketRun`]: each value it records may also
/// be found at its target, and a write cut off between the two is finished
/// by the next holder of the ticket's lock, whichever the ref or the branch
/// then shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunWriting {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_ref: Option<RunTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tip: Option<RunTarget>,
}

impl RunWriting {
    /// Whether no write is under way.
    pub fn is_empty(&self) -> bool {
        self.ticket_ref.is_none() && self.tip.is_none()
    }
}

/// Where a write under way goes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunTarget {
    /// The commit it moves to; none for a deletion.
    pub to: Option<String>,
}

/// The most bytes of a [`RunObligation`]'s description.
pub const MAX_OBLIGATION_WHAT: usize = 1024;

/// The longest worktree entry name a [`TicketRun`] records, in bytes.
pub const MAX_ENTRY: usize = 255;

impl TicketRun {
    const CONTRACT: &str = "ticket run";

    /// The run file of a ticket from nothing: generation 0, which the
    /// first write raises.
    pub fn new(ticket: TicketId) -> Self {
        Self {
            format: Format,
            ticket,
            generation: 0,
            tip: None,
            ticket_ref: None,
            entry: None,
            writing: RunWriting::default(),
        }
    }

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let run: Self = format::parse_json(Self::CONTRACT, RUN_FORMAT, input)?;
        let commits = [
            run.tip.as_deref(),
            run.ticket_ref.as_deref(),
            run.writing
                .ticket_ref
                .as_ref()
                .and_then(|t| t.to.as_deref()),
            run.writing.tip.as_ref().and_then(|t| t.to.as_deref()),
        ];
        if let Some(bad) = commits.into_iter().flatten().find(|id| !is_commit_id(id)) {
            return Err(ContractError::invalid(
                Self::CONTRACT,
                format!("{bad:?} is not a full commit id"),
            ));
        }
        if let Some(entry) = &run.entry
            && !is_component(entry, MAX_ENTRY)
        {
            return Err(ContractError::invalid(
                Self::CONTRACT,
                format!("the worktree entry {entry:?} is not one plain name"),
            ));
        }
        Ok(run)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

/// A run's promise to have its isolation checked (OWL-400; architecture
/// section 8, "Isolation among steps", "Obligations and the quarantine"):
/// written in `unverified/` of the project's folder before the run starts,
/// removed once its check passed. One left behind by a run that ended
/// without its check quarantines the project.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift run obligation")]
pub struct RunObligation {
    pub format: Format<OBLIGATION_FORMAT>,
    /// The ticket whose run it is; none for `owlshift gate`.
    #[serde(default)]
    pub ticket: Option<TicketId>,
    /// The run's id, unique within its ticket: ASCII letters, digits and
    /// `-`, at most [`MAX_ATTEMPT`] bytes.
    #[schemars(regex(pattern = r"^[A-Za-z0-9-]{1,80}$"))]
    pub run: String,
    pub started: Timestamp,
    /// What ran, in the runner's words, on one line of at most
    /// [`MAX_OBLIGATION_WHAT`] bytes.
    pub what: String,
}

impl RunObligation {
    const CONTRACT: &str = "run obligation";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let obligation: Self = format::parse_json(Self::CONTRACT, OBLIGATION_FORMAT, input)?;
        if !is_run_id(&obligation.run) {
            return Err(ContractError::invalid(
                Self::CONTRACT,
                format!("{:?} is not a run id", obligation.run),
            ));
        }
        if obligation.what.len() > MAX_OBLIGATION_WHAT
            || obligation.what.chars().any(char::is_control)
        {
            return Err(ContractError::invalid(
                Self::CONTRACT,
                "its description is not one short line",
            ));
        }
        Ok(obligation)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

/// The work in flight outside Owlshift that a pass of `owlshift watch
/// --drain` read (OWL-410), handed to the step of a dispatch it starts, so
/// that the step's admission weighs what the pass weighed (OWL-401;
/// architecture section 5, "Admission, one transaction"): kept in
/// `outside-<attempt>.json` in the ticket's folder, written by watch before
/// it starts the step's child, read and removed by the child.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift step outside")]
pub struct StepOutside {
    pub format: Format<OUTSIDE_FORMAT>,
    /// The repository's open pull requests, each with the files it changes.
    pub pulls: Vec<OutsidePull>,
    /// The tickets of the tracker's working state.
    pub working: Vec<OutsideTicket>,
}

/// An open pull request as a pass read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutsidePull {
    pub number: u64,
    pub head_branch: String,
    /// The files it changes, both paths of a rename; none when it holds the
    /// whole repository (a list too long or too large to read).
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

/// A ticket of the working state as a pass read it: what the collision rule
/// reads of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutsideTicket {
    pub id: TicketId,
    pub labels: Vec<String>,
}

impl StepOutside {
    const CONTRACT: &str = "step outside";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        format::parse_json(Self::CONTRACT, OUTSIDE_FORMAT, input)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

/// Whether `id` is a run's id: ASCII letters, digits and `-`, at most
/// [`MAX_ATTEMPT`] bytes.
pub fn is_run_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ATTEMPT
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Whether `id` is a full commit id as git prints it: 40 or 64 lower-case
/// hexadecimal digits.
pub fn is_commit_id(id: &str) -> bool {
    owlshift_core::floor::is_commit_id(id)
}

/// Whether `name` is one plain path component of at most `max` bytes.
fn is_component(name: &str, max: usize) -> bool {
    !name.is_empty()
        && name.len() <= max
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(char::is_control)
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
            kept: KeptFollowing {
                moved: Some("Backlog".to_owned()),
                unposted: Some(KeptUnposted {
                    commit: "b".repeat(40),
                    reason: "the tracker is down".to_owned(),
                }),
                paused_until: Some("2026-10-20T10:00:00Z".parse().unwrap()),
            },
        }
    }

    #[test]
    fn every_envelope_reads_back_as_written() {
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
            },
            StepKind::Correct,
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

    /// OWL-409: format 2 adds `no_slot`; a format-1 record a predecessor
    /// watch left still reads, as format 2, but never with that end.
    #[test]
    fn a_format_1_outcome_reads_and_no_slot_is_format_2s() {
        let outcome = StepOutcome {
            format: Format,
            ticket: TicketId::new("OWL-1").unwrap(),
            attempt: "1-2-3".to_owned(),
            commit: None,
            progress: "p".to_owned(),
            answer: None,
            end: StepEnd::NoSlot,
            after: RefAfter::Unread,
            hold: None,
            kept: KeptFollowing::default(),
            at: "2026-10-20T09:00:00Z".parse().unwrap(),
        };
        let as_format_1 = |outcome: &StepOutcome| {
            let mut document: serde_json::Value = serde_json::from_str(&outcome.render()).unwrap();
            assert_eq!(document["format"], 4);
            document["format"] = 1.into();
            document.to_string()
        };
        assert_eq!(StepOutcome::parse(&outcome.render()).unwrap(), outcome);
        let error = StepOutcome::parse(&as_format_1(&outcome)).unwrap_err();
        assert!(error.to_string().contains("no `no_slot`"), "{error}");
        let busy = StepOutcome {
            end: StepEnd::Busy,
            ..outcome
        };
        assert_eq!(StepOutcome::parse(&as_format_1(&busy)).unwrap(), busy);
    }

    /// OWL-404: format 3 drops the `paused` hold; a format-1 or format-2
    /// record that calls for one reads with no hold, the machine's usage
    /// state holding the harness instead, and `no_slot` reads in format 2.
    #[test]
    fn an_older_outcome_reads_with_its_paused_hold_dropped() {
        let outcome = StepOutcome {
            format: Format,
            ticket: TicketId::new("OWL-1").unwrap(),
            attempt: "1-2-3".to_owned(),
            commit: None,
            progress: "p".to_owned(),
            answer: None,
            end: StepEnd::UsageLimit {
                resets_at: Some("2026-10-20T12:00:00Z".parse().unwrap()),
            },
            after: RefAfter::Unread,
            hold: None,
            kept: KeptFollowing::default(),
            at: "2026-10-20T09:00:00Z".parse().unwrap(),
        };
        for format in [1, 2] {
            let mut document: serde_json::Value = serde_json::from_str(&outcome.render()).unwrap();
            document["format"] = format.into();
            document["hold"] =
                serde_json::json!({"hold": "paused", "until": "2026-10-20T12:00:00Z"});
            assert_eq!(
                StepOutcome::parse(&document.to_string()).unwrap(),
                outcome,
                "format {format}"
            );
        }
        let mut no_slot: serde_json::Value = serde_json::from_str(&outcome.render()).unwrap();
        no_slot["format"] = 2.into();
        no_slot["end"] = serde_json::json!({"end": "no_slot"});
        assert_eq!(
            StepOutcome::parse(&no_slot.to_string()).unwrap().end,
            StepEnd::NoSlot
        );
    }

    /// OWL-418: format 4 adds `base_held`; a format-3 record a predecessor
    /// watch left still reads, as format 4, but never with that end.
    #[test]
    fn base_held_is_format_4s() {
        let outcome = StepOutcome {
            format: Format,
            ticket: TicketId::new("OWL-1").unwrap(),
            attempt: "1-2-3".to_owned(),
            commit: None,
            progress: "p".to_owned(),
            answer: None,
            end: StepEnd::BaseHeld {
                commit: "a".repeat(40),
                command: Some("sh ci/owlshift/setup.sh".to_owned()),
                pending: true,
            },
            after: RefAfter::Unread,
            hold: None,
            kept: KeptFollowing::default(),
            at: "2026-10-22T09:00:00Z".parse().unwrap(),
        };
        assert_eq!(StepOutcome::parse(&outcome.render()).unwrap(), outcome);
        let as_format_3 = |outcome: &StepOutcome| {
            let mut document: serde_json::Value = serde_json::from_str(&outcome.render()).unwrap();
            document["format"] = 3.into();
            document.to_string()
        };
        let error = StepOutcome::parse(&as_format_3(&outcome)).unwrap_err();
        assert!(error.to_string().contains("no `base_held`"), "{error}");
        let busy = StepOutcome {
            end: StepEnd::Busy,
            ..outcome
        };
        assert_eq!(StepOutcome::parse(&as_format_3(&busy)).unwrap(), busy);
    }

    /// A run file and an obligation read back as written, a write under way
    /// included; a newer format, a short commit id, an entry that is a path
    /// and an obligation's run id that is one are refused.
    #[test]
    fn a_run_file_and_an_obligation_read_back_and_refuse_what_is_not_theirs() {
        let mut run = TicketRun::new(TicketId::new("OWL-3").unwrap());
        run.generation = 7;
        run.tip = Some("a".repeat(40));
        run.ticket_ref = Some("b".repeat(64));
        run.entry = Some("owl-3".to_owned());
        assert_eq!(TicketRun::parse(&run.render()).unwrap(), run);
        assert!(!run.render().contains("writing"), "{}", run.render());
        run.writing = RunWriting {
            ticket_ref: Some(RunTarget { to: None }),
            tip: Some(RunTarget {
                to: Some("c".repeat(40)),
            }),
        };
        assert_eq!(TicketRun::parse(&run.render()).unwrap(), run);

        let edit = |change: &dyn Fn(&mut serde_json::Value)| {
            let mut document: serde_json::Value = serde_json::from_str(&run.render()).unwrap();
            change(&mut document);
            TicketRun::parse(&document.to_string())
        };
        assert!(matches!(
            edit(&|d| d["format"] = 2.into()),
            Err(ContractError::NewerFormat { found: 2, .. })
        ));
        assert!(edit(&|d| d["tip"] = "abc123".into()).is_err());
        assert!(edit(&|d| d["writing"]["tip"]["to"] = "A".repeat(40).into()).is_err());
        for entry in ["../x", "a/b", "", "..", "a\nb"] {
            assert!(edit(&|d| d["entry"] = entry.into()).is_err(), "{entry:?}");
        }

        let obligation = RunObligation {
            format: Format,
            ticket: Some(TicketId::new("OWL-3").unwrap()),
            run: "20261021T080318Z-2".to_owned(),
            started: "2026-10-21T08:03:18Z".parse().unwrap(),
            what: "run 20261021T080318Z-2 of OWL-3".to_owned(),
        };
        assert_eq!(
            RunObligation::parse(&obligation.render()).unwrap(),
            obligation
        );
        let gate = RunObligation {
            ticket: None,
            ..obligation.clone()
        };
        assert_eq!(RunObligation::parse(&gate.render()).unwrap(), gate);
        for (run, what) in [("../x", "w"), ("r", "two\nlines")] {
            let bad = RunObligation {
                run: run.to_owned(),
                what: what.to_owned(),
                ..obligation.clone()
            };
            assert!(RunObligation::parse(&bad.render()).is_err(), "{run:?}");
        }
    }

    /// OWL-401: the outside work a pass hands a dispatch's step, a list of
    /// files or the whole repository for each pull request, round trips.
    #[test]
    fn the_outside_work_round_trips() {
        let outside = StepOutside {
            format: Format,
            pulls: vec![
                OutsidePull {
                    number: 7,
                    head_branch: "someone/fix".to_owned(),
                    files: Some(vec!["a.md".to_owned()]),
                },
                OutsidePull {
                    number: 8,
                    head_branch: "huge".to_owned(),
                    files: None,
                },
            ],
            working: vec![OutsideTicket {
                id: TicketId::new("DEMO-9").unwrap(),
                labels: vec!["zone:app".to_owned()],
            }],
        };
        assert_eq!(StepOutside::parse(&outside.render()).unwrap(), outside);
        assert!(StepOutside::parse("{\"format\": 2, \"pulls\": [], \"working\": []}").is_err());
    }
}
