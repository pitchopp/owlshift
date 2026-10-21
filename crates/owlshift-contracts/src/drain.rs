//! The drain's documents in a ticket ref (OWL-245, architecture section 5,
//! "The drain"): the intake record, [`TicketIntake`] in
//! [`INTAKE_FILE`](crate::refs::INTAKE_FILE), and the follow-ups the ticket's
//! runs proposed, [`TicketProposals`] in
//! [`PROPOSALS_FILE`](crate::refs::PROPOSALS_FILE). Each is read when
//! present and written only when it has content: `owlshift forget` drops the
//! intake record, so the next pass runs intake again, and keeps the
//! proposals, so nothing is filed twice.

use std::num::NonZeroU32;

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Variant;
use crate::format::{self, ContractError, Format, INTAKE_FORMAT, PROPOSALS_FORMAT};
use crate::ids::{TicketId, check_name, is_lower_hex};
use crate::refs::PushKind;
use crate::result::{
    Followup, MAX_DECLARED, MAX_DEPENDENCIES, Routing, check_declaration, check_dependencies,
};

/// The intake record of a ticket: the text intake read, the gesture that
/// admitted the ticket, the verdict and the dependencies with what became of
/// each. Intake runs again when the ticket's text no longer has this hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket intake")]
pub struct TicketIntake {
    pub format: Format<INTAKE_FORMAT>,
    /// The SHA-256 of the ticket's title, a NUL byte and its description, as
    /// 64 lowercase hexadecimal digits.
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub text_sha256: String,
    pub gesture: Gesture,
    pub variant: Variant,
    pub routing: Routing,
    /// The dependencies intake found, at most
    /// [`MAX_DEPENDENCIES`].
    #[serde(default)]
    #[schemars(length(max = MAX_DEPENDENCIES))]
    pub dependencies: Vec<KeptDependency>,
    /// The zones intake declared, as it wrote them (format 2, OWL-402): at
    /// most [`MAX_DECLARED`]. None: the ticket holds the whole repository.
    /// The zones the labels add and the resources the zones reach are not
    /// kept: they are read again for each brief.
    #[serde(default)]
    #[schemars(length(max = MAX_DECLARED))]
    pub zones: Vec<String>,
    /// The named resources intake declared (format 2, OWL-402): at most
    /// [`MAX_DECLARED`].
    #[serde(default)]
    #[schemars(length(max = MAX_DECLARED))]
    pub resources: Vec<String>,
}

/// The admission gesture, read just before the ticket's first intake: the
/// label put on it or the state it was moved to, the tracker account that
/// made it, and when the tracker says it was made. Later passes ask only
/// that the gesture is still on the ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Gesture {
    pub kind: GestureKind,
    /// The label's or the state's name, as `[tracker].admit` names it.
    pub name: String,
    /// The account's identifier as the tracker reports it, one of
    /// `[tracker].admitters`.
    pub account: String,
    /// The tracker's time of the gesture.
    pub at: Timestamp,
}

/// Which gesture admitted the ticket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GestureKind {
    Label,
    State,
}

/// The quote of a dependency the runner adds to an intake record because a
/// `blocks` relation on the tracker already held the ticket (OWL-416,
/// architecture section 5, "Dependencies to relations"): not words of the
/// ticket, which may name the prerequisite in words only or not at all.
pub const RELATION_QUOTE: &str = "a blocks relation on the tracker";

/// A dependency intake found, with what became of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptDependency {
    pub ticket: TicketId,
    /// The words of the ticket that name it; for a ticket that already
    /// blocked it on the tracker, which the runner adds without the text
    /// naming it (OWL-416), [`RELATION_QUOTE`].
    #[schemars(regex(pattern = r"\S"))]
    pub quote: String,
    pub outcome: DependencyOutcome,
}

/// What became of a dependency (architecture section 5, "Dependencies to
/// relations"): a `blocks` relation written from it to the ticket, or why
/// none was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DependencyOutcome {
    /// The runner wrote the relation.
    Written,
    /// The relation was already there.
    Existed,
    /// The dependency is already done.
    Done,
    /// The tracker knows no such ticket.
    Unknown,
    /// It names the ticket itself.
    Itself,
    /// The dependency is already blocked by the ticket, directly or through
    /// its own blockers.
    Cycle,
    /// Not checked yet: intake records every dependency so, and the relation
    /// write (part F) turns it into one of the outcomes above.
    Pending,
}

impl TicketIntake {
    const CONTRACT: &str = "ticket intake";

    /// Parses and validates an `intake.json`. A format-1 one, kept before
    /// intake declared zones (OWL-402), is read as the current format with
    /// no zone and no resource: its ticket holds the whole repository until
    /// intake runs again, after an edit of its text or `owlshift forget`.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let intake: Self = match Self::from_older_format(input) {
            Some(read) => read?,
            None => format::parse_json(Self::CONTRACT, INTAKE_FORMAT, input)?,
        };
        intake.validate()?;
        Ok(intake)
    }

    /// A format-1 document read as the current format, or `None` for any
    /// other. Format 1 is format 2 without `zones` and `resources`, which
    /// are refused in it.
    fn from_older_format(input: &str) -> Option<Result<Self, ContractError>> {
        let mut document: serde_json::Value = serde_json::from_str(input).ok()?;
        if document.get("format").and_then(serde_json::Value::as_u64) != Some(1) {
            return None;
        }
        if document.get("zones").is_some() || document.get("resources").is_some() {
            return Some(Err(ContractError::invalid(
                Self::CONTRACT,
                "format 1 keeps no zones or resources",
            )));
        }
        document["format"] = INTAKE_FORMAT.into();
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

    /// Checks the hash is a SHA-256 in lowercase hex, the gesture's name
    /// and account are names written exactly (not blank, unpadded, without
    /// a control character), and the dependencies, zones and resources keep
    /// the rules of a result's.
    pub fn validate(&self) -> Result<(), ContractError> {
        let invalid = |reason: String| Err(ContractError::invalid(Self::CONTRACT, reason));
        if !is_lower_hex(&self.text_sha256, 64) {
            return invalid(format!(
                "text_sha256 {:?} is not a SHA-256 in lowercase hex",
                self.text_sha256
            ));
        }
        for (field, name) in [
            ("gesture.name", &self.gesture.name),
            ("gesture.account", &self.gesture.account),
        ] {
            if let Err(broken) = check_name(name) {
                return invalid(format!("{field} {name:?} {broken}"));
            }
        }
        check_dependencies(
            Self::CONTRACT,
            self.dependencies
                .iter()
                .map(|d| (&d.ticket, d.quote.as_str())),
        )?;
        check_declaration(Self::CONTRACT, &self.zones, &self.resources)
    }
}

/// The follow-ups the ticket's runs proposed, owed until filed, then kept
/// with what became of them (architecture section 5, "Follow-ups").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket proposals")]
pub struct TicketProposals {
    pub format: Format<PROPOSALS_FORMAT>,
    /// At least one: a ref keeps no `proposals.json` without a proposal.
    #[schemars(length(min = 1))]
    pub proposals: Vec<KeptProposal>,
}

/// One proposed follow-up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptProposal {
    /// The first 12 lowercase hexadecimal digits of the SHA-256 of its
    /// normalized title, as its footer shows them.
    #[schemars(regex(pattern = r"^[0-9a-f]{12}$"))]
    pub fingerprint: String,
    /// The follow-up as the run proposed it, kept whole so that a filing
    /// the tracker refused can be made again by a later pass.
    pub proposal: Followup,
    pub run: ProposingRun,
    pub outcome: ProposalOutcome,
}

/// The run that proposed a follow-up: the delivering Build, or a fix,
/// rebase, correction or, since format 2, review loop run whose push
/// landed, counted as its push is in `delivery.json`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposingRun {
    pub kind: PushKind,
    /// The fix pass or the repair the run was, from 1; 1 for the delivery.
    pub count: NonZeroU32,
}

/// What became of a proposal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProposalOutcome {
    /// Not filed yet; with why the tracker refused the last filing, if one
    /// was tried.
    Owed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refusal: Option<String>,
    },
    /// Filed as this ticket.
    Filed { ticket: TicketId },
    /// Already proposed as this ticket, so not filed.
    Duplicate { ticket: TicketId },
    /// Not filed: the parent had its three filed already.
    OverLimit,
}

impl TicketProposals {
    const CONTRACT: &str = "ticket proposals";

    /// Parses and validates a `proposals.json`. A format-1 one, written
    /// before a review loop's run could propose (OWL-300), is read as the
    /// current format, and the next write gives it that format.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let proposals: Self = match Self::from_older_format(input) {
            Some(read) => read?,
            None => format::parse_json(Self::CONTRACT, PROPOSALS_FORMAT, input)?,
        };
        proposals.validate()?;
        Ok(proposals)
    }

    /// A format-1 document read as the current format, or `None` for any
    /// other. Format 1 is format 2 without a review loop's run, which is
    /// refused in it.
    fn from_older_format(input: &str) -> Option<Result<Self, ContractError>> {
        let mut document: serde_json::Value = serde_json::from_str(input).ok()?;
        if document.get("format").and_then(serde_json::Value::as_u64) != Some(1) {
            return None;
        }
        let review = document["proposals"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|proposal| proposal["run"]["kind"] == "review");
        if review {
            return Some(Err(ContractError::invalid(
                Self::CONTRACT,
                "format 1 keeps no review loop's run",
            )));
        }
        document["format"] = PROPOSALS_FORMAT.into();
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

    /// Checks there is a proposal, and each fingerprint is 12 lowercase
    /// hexadecimal digits.
    pub fn validate(&self) -> Result<(), ContractError> {
        let invalid = |reason: String| Err(ContractError::invalid(Self::CONTRACT, reason));
        if self.proposals.is_empty() {
            return invalid("no proposal is kept".to_owned());
        }
        if let Some(kept) = self
            .proposals
            .iter()
            .find(|p| !is_lower_hex(&p.fingerprint, 12))
        {
            return invalid(format!(
                "fingerprint {:?} is not 12 lowercase hexadecimal digits",
                kept.fingerprint
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn intake() -> serde_json::Value {
        json!({
            "format": 2,
            "text_sha256": "a".repeat(64),
            "gesture": { "kind": "label", "name": "agent", "account": "u-ann", "at": "2026-10-13T08:00:00Z" },
            "variant": "standard",
            "routing": { "harness": "claude", "tier": "standard", "effort": "high" },
            "dependencies": [{ "ticket": "OWL-1", "quote": "Blocked by OWL-1.", "outcome": "written" }],
            "zones": ["backend/billing", "docs/"],
            "resources": ["db.migrations"]
        })
    }

    /// OWL-402: a format-1 record, kept before intake declared zones, is
    /// read with none, so its ticket holds the whole repository; one that
    /// holds zones is refused.
    #[test]
    fn a_format_one_intake_record_is_read_as_holding_the_whole_repository() {
        let mut older = intake();
        older["format"] = 1.into();
        let fields = older.as_object_mut().unwrap();
        fields.remove("zones");
        fields.remove("resources");
        let read = TicketIntake::parse(&older.to_string()).unwrap();
        assert!(read.zones.is_empty() && read.resources.is_empty());
        assert!(read.render().contains("\"format\": 2"), "{}", read.render());
        older["zones"] = json!(["docs"]);
        let error = TicketIntake::parse(&older.to_string()).unwrap_err();
        assert!(
            error.to_string().contains("format 1 keeps no zones"),
            "{error}"
        );
    }

    fn proposals() -> serde_json::Value {
        json!({
            "format": 1,
            "proposals": [{
                "fingerprint": "0123456789ab",
                "proposal": {
                    "title": "Add X", "why": "Y", "source": "agent", "done_when": "Z"
                },
                "run": { "kind": "fix", "count": 2 },
                "outcome": { "kind": "owed", "refusal": "the plan holds 250 issues" }
            }]
        })
    }

    #[test]
    fn an_intake_record_round_trips_and_refuses_what_it_must() {
        let parsed = TicketIntake::parse(&intake().to_string()).unwrap();
        assert_eq!(parsed.gesture.kind, GestureKind::Label);
        assert_eq!(parsed.dependencies[0].outcome, DependencyOutcome::Written);
        assert_eq!(TicketIntake::parse(&parsed.render()).unwrap(), parsed);
        // Intake records every dependency `pending` until part F checks it.
        let mut pending = intake();
        pending["dependencies"][0]["outcome"] = "pending".into();
        let pending = TicketIntake::parse(&pending.to_string()).unwrap();
        assert_eq!(pending.dependencies[0].outcome, DependencyOutcome::Pending);

        let refused = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut document = intake();
            edit(&mut document);
            TicketIntake::parse(&document.to_string())
                .unwrap_err()
                .to_string()
        };
        for (edit, reason) in [
            (
                &(|d: &mut serde_json::Value| d["text_sha256"] = "A".repeat(64).into())
                    as &dyn Fn(&mut serde_json::Value),
                "not a SHA-256",
            ),
            (
                &|d| d["gesture"]["account"] = " u-ann".into(),
                "gesture.account \" u-ann\" has a space",
            ),
            (
                &|d| d["gesture"]["name"] = "".into(),
                "gesture.name \"\" is blank",
            ),
            (
                &|d| d["dependencies"][0]["quote"] = "a\nb".into(),
                "control character",
            ),
            (
                &|d| d["dependencies"][0]["outcome"] = "owed".into(),
                "unknown variant",
            ),
            (&|d| d["format"] = 3.into(), "upgrade Owlshift"),
            (
                &|d| d["zones"][1] = "../x".into(),
                "the zone \"../x\" is not a folder's path",
            ),
            (
                &|d| d["zones"][0] = "db/*/migrations".into(),
                "holds `*`: a zone is a folder, not a pattern",
            ),
            (
                &|d| d["zones"][0] = "".into(),
                "the zone \"\" is the whole repository",
            ),
            (
                &|d| d["resources"][0] = "db migrations".into(),
                "the resource \"db migrations\" may hold only",
            ),
            (
                &|d| d["zones"] = json!(vec!["docs"; MAX_DECLARED + 1]),
                "65 zones are declared, past the 64",
            ),
        ] {
            let error = refused(edit);
            assert!(error.contains(reason), "{reason}: {error}");
        }
    }

    #[test]
    fn proposals_round_trip_and_refuse_what_they_must() {
        let parsed = TicketProposals::parse(&proposals().to_string()).unwrap();
        assert_eq!(parsed.proposals[0].run.kind, PushKind::Fix);
        assert_eq!(TicketProposals::parse(&parsed.render()).unwrap(), parsed);
        for outcome in [
            json!({ "kind": "owed" }),
            json!({ "kind": "filed", "ticket": "OWL-9" }),
            json!({ "kind": "duplicate", "ticket": "OWL-8" }),
            json!({ "kind": "over_limit" }),
        ] {
            let mut document = proposals();
            document["proposals"][0]["outcome"] = outcome.clone();
            TicketProposals::parse(&document.to_string())
                .unwrap_or_else(|e| panic!("{outcome}: {e}"));
        }

        for (edit, reason) in [
            (
                &(|d: &mut serde_json::Value| d["proposals"] = json!([]))
                    as &dyn Fn(&mut serde_json::Value),
                "no proposal is kept",
            ),
            (
                &|d| d["proposals"][0]["fingerprint"] = "0123456789AB".into(),
                "not 12 lowercase",
            ),
            (&|d| d["proposals"][0]["run"]["count"] = 0.into(), "nonzero"),
            (
                &|d| {
                    d["proposals"][0]["outcome"] =
                        json!({ "kind": "filed", "ticket": "OWL-9", "x": 1 })
                },
                "unknown field",
            ),
        ] {
            let mut document = proposals();
            edit(&mut document);
            let error = TicketProposals::parse(&document.to_string())
                .unwrap_err()
                .to_string();
            assert!(error.contains(reason), "{reason}: {error}");
        }
    }
}
