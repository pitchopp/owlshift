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

/// The format version of the brief.
pub const BRIEF_FORMAT: u32 = 2;
/// The format version of `result.json`.
pub const RESULT_FORMAT: u32 = 1;
/// The format version of an event.
pub const EVENT_FORMAT: u32 = 1;
/// The format version of a claim (`claim.json` on a claim ref).
pub const CLAIM_FORMAT: u32 = 1;
/// The format version of a ticket's state (`state.json` on a ticket ref).
pub const TICKET_STATE_FORMAT: u32 = 2;
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
    Toml {
        contract: &'static str,
        source: toml::de::Error,
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
            Self::Toml { source, .. } => Some(source),
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
            source,
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
