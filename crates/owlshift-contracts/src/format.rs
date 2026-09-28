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

/// The format version of the brief.
pub const BRIEF_FORMAT: u32 = 1;
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
