//! Format versions and the parsing entry point shared by the JSON contracts.
//!
//! Every JSON contract carries a `format` number. A reader peeks at it before
//! anything else, so a document written by a newer Owlshift fails with an
//! "upgrade" error instead of an unknown-field error about a shape this binary
//! has never seen.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, DeserializeOwned};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Role;

/// The format version of the brief. Format 4 added the resolver's `resolve`
/// field and the thread's `decision` entries (OWL-138); format 5 leaves the
/// raising run's category out of `resolve` (OWL-144); format 6 adds the
/// project's `always_human` categories (OWL-151); format 7 Build's
/// `result_refusal` (OWL-180); format 8 gives it to the answer check too
/// (OWL-184); format 9 adds Build's `decisions_refused` (OWL-186); format 10
/// gives `result_refusal` to the resolver too (OWL-191); format 11 adds
/// Build's `refused_choices` (OWL-198); format 12 adds Build's `check_failure`
/// (OWL-112); format 13 the rebase role's `rebase` (OWL-210); format 14
/// Build's `correction` and a thread decision's `reversed` (OWL-146); format
/// 15 the quality pipeline's `plan`, `approval`, `findings`, `review` and
/// `pull_request_reviews`, and a question's `replan` (OWL-300); format 16 a
/// question's lettered options and structured recommendation (OWL-324);
/// format 17 the resolver's `delegation` (OWL-337); format 18 the
/// resolver's `asked_by` and a question's `finding` in `resolve` (OWL-343);
/// format 19 the thread's `answered` entries and a decision's short
/// question (OWL-359); format 20 leaves the plan's `forks` and
/// `alternatives` out of Build's brief (OWL-365); format 21 adds a review
/// run's `since` and `earlier` and Build's findings to `note` (OWL-356);
/// format 22 the author's `disagreed` and `deferred` answers, beside a
/// confirmed ruling, in `earlier`, and answers to findings to `fix`
/// (OWL-377).
pub const BRIEF_FORMAT: u32 = 22;
/// The format version of `result.json`. Format 4 added the resolver's
/// `resolutions` (OWL-138); format 5 the resolver's own `category` on each
/// decision (OWL-144); format 6 refuses `decisions` from the build role
/// (OWL-176); format 7 adds Build's `refused_choices_asked` (OWL-193);
/// format 8 the intake role's `intake` (OWL-245); format 9 the quality
/// pipeline's `plan`, `approval`, `findings`, `rulings` and
/// `finding_responses`, a question's `replan`, and `premise_false` from the
/// intake, design and build roles only (OWL-300); format 10 a question's
/// options as `{label, detail}` and its required `recommendation` as
/// `{option, why}` (OWL-324); format 11 the resolver's `basis_kind` on each
/// decision, and its `quote` on a `ticket` one (OWL-337); format 12 the
/// `judgment` basis kind and a decision's `option` (OWL-343); format 13
/// `finding_responses` to findings to `fix` too, and their `disagreed` and
/// `deferred` outcomes (OWL-377).
pub const RESULT_FORMAT: u32 = 13;
/// The format version of an event. Format 2 added the `warning` kind (OWL-134);
/// the log is append-only, so [`Event::parse`](crate::event::Event::parse)
/// still reads the format-1 lines already written.
pub const EVENT_FORMAT: u32 = 2;
/// The format version of a claim (`claim.json` on a claim ref).
pub const CLAIM_FORMAT: u32 = 1;
/// The format version of a ticket's state (`state.json` on a ticket ref).
/// Format 3 adds the fix passes and repairs at Watch (OWL-55); format 4 the
/// review passes of the current stage (OWL-300); formats 2 and 3 are still
/// read, with what they lack at 0.
pub const TICKET_STATE_FORMAT: u32 = 4;
/// The format version of a ticket's asks (`questions.json` on a ticket ref).
/// Format 4 added the resolver's kept `decisions` (OWL-138); format 5 an
/// ask's `result_refusal` (OWL-184); format 6 Build's kept `build_refusal`
/// (OWL-192); format 7 the resolver's kept `resolver_refusal` (OWL-191);
/// format 8 every refused list of choices in `build_refusal` (OWL-195);
/// format 9 the refused choices typed in it (OWL-198); format 10 a
/// decision's `reversed` (OWL-146); format 11 a question's `replan`
/// (OWL-300); format 12 a question's options as `{label, detail}` and its
/// recommendation as `{option, why}` (OWL-324); format 13 a decision's
/// `basis_kind` (OWL-343); format 14 a decision's `asked_by` (OWL-345);
/// format 15 a decision's `number` in its DECISION comment and the
/// `decisions_document` (OWL-351); format 16 a decision's `recommendation`
/// basis kind (OWL-375); formats 2 to 15 are still read.
pub const QUESTIONS_FORMAT: u32 = 16;
/// The format version of a ticket's delivery (`delivery.json` on a ticket
/// ref, OWL-209). Format 2 adds the `owed` report and the push's kept
/// `report` (OWL-218); format 3 a correction run's push (OWL-146); format 4
/// the first delivery's owed report, with its kept `notes`, and a
/// correction's owed report that pushed nothing (OWL-236); format 5 a review
/// loop's push and its report's answered `reviews` (OWL-300); formats 1 to 4
/// are still read.
pub const DELIVERY_FORMAT: u32 = 5;
/// The format version of a ticket's intake record (`intake.json` on a ticket
/// ref, OWL-245).
pub const INTAKE_FORMAT: u32 = 1;
/// The format version of a ticket's proposed follow-ups (`proposals.json` on
/// a ticket ref, OWL-245). Format 2 lets a review loop's run propose
/// (OWL-300); format 1 is still read.
pub const PROPOSALS_FORMAT: u32 = 2;
/// The format version of a ticket's plan (`plan.json` on a ticket ref,
/// OWL-300).
pub const PLAN_FORMAT: u32 = 1;
/// The format version of a ticket's review record (`review.json` on a ticket
/// ref, OWL-300).
pub const REVIEW_FORMAT: u32 = 1;
/// The format version of a marked comment's footer.
pub const FOOTER_FORMAT: u32 = 1;

/// Why a contract document was refused.
#[derive(Debug)]
pub enum ContractError {
    /// The document declares a format newer than this binary understands.
    NewerFormat {
        contract: &'static str,
        found: u64,
        supported: u32,
    },
    /// The document is not valid JSON for the contract, or breaks its shape.
    Json {
        contract: &'static str,
        source: serde_json::Error,
    },
    /// The document is not valid TOML for the contract, or breaks its shape.
    /// Boxed: the TOML error is large, and every contract's result carries
    /// this type.
    Toml {
        contract: &'static str,
        source: Box<toml::de::Error>,
    },
    /// The document parses but breaks a rule of the contract.
    Invalid {
        contract: &'static str,
        reason: String,
    },
}

impl ContractError {
    pub(crate) fn invalid(contract: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            contract,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NewerFormat {
                contract,
                found,
                supported,
            } => write!(
                f,
                "{contract} has format {found}, written by a newer Owlshift; this binary reads \
                 format {supported}: upgrade Owlshift"
            ),
            Self::Json { contract, source } => write!(f, "invalid {contract}: {source}"),
            Self::Toml { contract, source } => write!(f, "invalid {contract}: {source}"),
            Self::Invalid { contract, reason } => write!(f, "invalid {contract}: {reason}"),
        }
    }
}

impl std::error::Error for ContractError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json { source, .. } => Some(source),
            Self::Toml { source, .. } => Some(source.as_ref()),
            Self::NewerFormat { .. } | Self::Invalid { .. } => None,
        }
    }
}

/// The `format` field of a contract at version `N`: it serializes as `N` and
/// deserializes only from `N`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Format<const N: u32>;

impl<const N: u32> Format<N> {
    /// The version number this field carries.
    pub const VERSION: u32 = N;
}

impl<const N: u32> Serialize for Format<N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(N)
    }
}

impl<'de, const N: u32> Deserialize<'de> for Format<N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let found = u64::deserialize(deserializer)?;
        if found == u64::from(N) {
            Ok(Format)
        } else if found > u64::from(N) {
            Err(de::Error::custom(format_args!(
                "format {found} was written by a newer Owlshift; this binary reads format {N}: \
                 upgrade Owlshift"
            )))
        } else {
            Err(de::Error::custom(format_args!(
                "unsupported format {found}; this binary reads format {N}"
            )))
        }
    }
}

impl<const N: u32> JsonSchema for Format<N> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        format!("Format{N}").into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "The format version of this document.",
            "type": "integer",
            "const": N,
        })
    }
}

/// Only the `format` field, read leniently: other fields are ignored.
#[derive(Deserialize)]
struct FormatPeek {
    format: Option<serde_json::Value>,
}

/// Parses a JSON contract document: format check first, then the strict parse.
///
/// A `format` above `supported` gives [`ContractError::NewerFormat`]; any other
/// value than `supported` is refused as invalid. A document that is not JSON,
/// or whose `format` is missing or not a number, falls through to the strict
/// parse, whose error names the problem.
pub(crate) fn parse_json<T: DeserializeOwned>(
    contract: &'static str,
    supported: u32,
    input: &str,
) -> Result<T, ContractError> {
    if let Ok(FormatPeek {
        format: Some(format),
    }) = serde_json::from_str::<FormatPeek>(input)
        && let Some(found) = format.as_u64()
    {
        if found > u64::from(supported) {
            return Err(ContractError::NewerFormat {
                contract,
                found,
                supported,
            });
        }
        if found != u64::from(supported) {
            return Err(ContractError::invalid(
                contract,
                format!("unknown format {found}; this binary reads format {supported}"),
            ));
        }
    }
    serde_json::from_str(input).map_err(|source| ContractError::Json { contract, source })
}

/// Renders a contract document as pretty JSON with a final newline.
pub(crate) fn render_json<T: serde::Serialize>(value: &T) -> String {
    let mut out =
        serde_json::to_string_pretty(value).expect("contract types always serialize to JSON");
    out.push('\n');
    out
}

/// The `+++`-delimited TOML front matter atop a role prompt file: the role it
/// was written for, and the brief/result format versions it names.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleFrontMatter {
    role: Role,
    brief_format: u32,
    result_format: u32,
}

/// Splits a role prompt's `+++` TOML front matter from the body handed to the
/// model, without its leading blank line — the same `+++` convention as
/// `owlshift_adapters::tracker::markdown`'s ticket front matter. TOML allows
/// either LF or CRLF line endings, so a checkout that turns line endings into
/// CRLF needs no separate handling here.
fn split_front_matter(input: &str) -> Result<(&str, &str), ContractError> {
    const CONTRACT: &str = "role prompt";
    let mut lines = input.split_inclusive('\n');
    let first = lines.next().unwrap_or_default();
    if first.trim_end() != "+++" {
        return Err(ContractError::invalid(
            CONTRACT,
            "the prompt does not open with a `+++` line",
        ));
    }
    let mut offset = first.len();
    for line in lines {
        if line.trim_end() == "+++" {
            let front = &input[first.len()..offset];
            let body = &input[offset + line.len()..];
            return Ok((front, body.trim_start_matches(['\r', '\n'])));
        }
        offset += line.len();
    }
    Err(ContractError::invalid(
        CONTRACT,
        "the prompt's front matter is not closed by a `+++` line",
    ))
}

/// Strips a role prompt file's `+++` front matter and returns the prompt text
/// handed to the model.
///
/// The front matter names the role and the brief/result format versions the
/// prompt was written for. A value that does not match `expected_role` or
/// this binary's [`BRIEF_FORMAT`]/[`RESULT_FORMAT`] is refused, so a role
/// file written for a stale contract fails loudly instead of silently
/// confusing the agent.
pub fn strip_role_front_matter(expected_role: Role, input: &str) -> Result<String, ContractError> {
    const CONTRACT: &str = "role prompt";
    let (front, body) = split_front_matter(input)?;
    let front_matter: RoleFrontMatter =
        toml::from_str(front).map_err(|source| ContractError::Toml {
            contract: CONTRACT,
            source: Box::new(source),
        })?;
    if front_matter.role != expected_role {
        return Err(ContractError::invalid(
            CONTRACT,
            format!(
                "the front matter names role {:?}, expected {:?}",
                front_matter.role, expected_role
            ),
        ));
    }
    if front_matter.brief_format != BRIEF_FORMAT {
        return Err(ContractError::invalid(
            CONTRACT,
            format!(
                "the front matter names brief_format {}, this binary writes {BRIEF_FORMAT}",
                front_matter.brief_format
            ),
        ));
    }
    if front_matter.result_format != RESULT_FORMAT {
        return Err(ContractError::invalid(
            CONTRACT,
            format!(
                "the front matter names result_format {}, this binary writes {RESULT_FORMAT}",
                front_matter.result_format
            ),
        ));
    }
    Ok(body.to_owned())
}

#[cfg(test)]
mod role_prompt_tests {
    use super::*;

    /// A well-formed role prompt file, built from this binary's own format
    /// constants so it never drifts from them.
    fn valid() -> String {
        format!(
            "+++\nrole = \"build\"\nbrief_format = {BRIEF_FORMAT}\nresult_format = {RESULT_FORMAT}\n+++\n\n# Build\n\nDo it.\n"
        )
    }

    #[test]
    fn strip_role_front_matter_returns_the_body() {
        let body = strip_role_front_matter(Role::Build, &valid()).expect("valid front matter");
        assert_eq!(body, "# Build\n\nDo it.\n");
    }

    #[test]
    fn strip_role_front_matter_refuses_a_mismatched_brief_format() {
        let input = valid().replace(
            &format!("brief_format = {BRIEF_FORMAT}"),
            &format!("brief_format = {}", BRIEF_FORMAT + 1),
        );
        let err = strip_role_front_matter(Role::Build, &input).unwrap_err();
        assert!(err.to_string().contains("brief_format"), "{err}");
    }

    #[test]
    fn strip_role_front_matter_refuses_a_mismatched_result_format() {
        let input = valid().replace(
            &format!("result_format = {RESULT_FORMAT}"),
            &format!("result_format = {}", RESULT_FORMAT + 1),
        );
        let err = strip_role_front_matter(Role::Build, &input).unwrap_err();
        assert!(err.to_string().contains("result_format"), "{err}");
    }

    #[test]
    fn strip_role_front_matter_refuses_a_mismatched_role() {
        let err = strip_role_front_matter(Role::Design, &valid()).unwrap_err();
        assert!(err.to_string().contains("Design"), "{err}");
    }

    #[test]
    fn strip_role_front_matter_refuses_a_missing_opening_delimiter() {
        let err = strip_role_front_matter(Role::Build, "# Build\n").unwrap_err();
        assert!(err.to_string().contains("open"), "{err}");
    }

    #[test]
    fn strip_role_front_matter_refuses_an_unclosed_front_matter() {
        let err = strip_role_front_matter(Role::Build, "+++\nrole = \"build\"\n").unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
    }

    #[test]
    fn strip_role_front_matter_refuses_an_unknown_key() {
        let input = valid().replace(
            &format!("result_format = {RESULT_FORMAT}"),
            &format!("result_format = {RESULT_FORMAT}\nextra = true"),
        );
        let err = strip_role_front_matter(Role::Build, &input).unwrap_err();
        assert!(matches!(err, ContractError::Toml { .. }), "{err}");
    }
}
