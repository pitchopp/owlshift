//! Validated identifiers and paths carried by several contracts.

use std::borrow::Cow;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

/// The JSON Schema pattern of a [`TicketId`].
pub const TICKET_ID_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$";
/// The JSON Schema pattern of a [`QuestionId`].
pub const QUESTION_ID_PATTERN: &str = "^Q[1-9][0-9]*$";
/// The JSON Schema pattern of a [`RelativePath`]: `/`-separated segments,
/// none empty or `..`, and no backslash or colon anywhere.
pub const RELATIVE_PATH_PATTERN: &str = r"^(?:[^/\\:.][^/\\:]*|\.(?:[^/\\:.][^/\\:]*)?|\.\.[^/\\:]+)(?:/(?:[^/\\:.][^/\\:]*|\.(?:[^/\\:.][^/\\:]*)?|\.\.[^/\\:]+))*$";

/// A tracker's ticket identifier, safe to use as a git ref component and a
/// file name: an ASCII letter or digit, then up to 63 letters, digits, `_` or
/// `-` (`OWL-12`, `PROJ-1`, `123`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TicketId(String);

impl TicketId {
    pub fn new(id: impl Into<String>) -> Result<Self, IdError> {
        let id = id.into();
        let mut chars = id.chars();
        let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
        let rest_ok = chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if first_ok && rest_ok && id.len() <= 64 {
            Ok(Self(id))
        } else {
            Err(IdError {
                kind: "ticket id",
                value: id,
                pattern: TICKET_ID_PATTERN,
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A question's identifier within a round: `Q1`, `Q2`, and so on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct QuestionId(String);

impl QuestionId {
    pub fn new(id: impl Into<String>) -> Result<Self, IdError> {
        let id = id.into();
        let valid = id.strip_prefix('Q').is_some_and(|n| {
            n.bytes().all(|b| b.is_ascii_digit()) && !n.starts_with('0') && n.parse::<u32>().is_ok()
        });
        if valid {
            Ok(Self(id))
        } else {
            Err(IdError {
                kind: "question id",
                value: id,
                pattern: QUESTION_ID_PATTERN,
            })
        }
    }

    /// The question for position `n` (1-based) in a round.
    pub fn nth(n: u32) -> Self {
        assert!(n > 0, "question numbers start at 1");
        Self(format!("Q{n}"))
    }

    /// The question's number: 1 for `Q1`.
    pub fn number(&self) -> u32 {
        self.0[1..].parse().expect("validated at construction")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A path relative to the worktree, from untrusted input, that cannot leave
/// it: `/`-separated segments, none empty or `..`. A backslash or a colon is
/// refused anywhere, which also rules out absolute, drive-prefixed and UNC
/// paths on every platform.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelativePath(String);

impl RelativePath {
    pub fn new(path: impl Into<String>) -> Result<Self, IdError> {
        let path = path.into();
        let valid = !path.contains(['\\', ':'])
            && path
                .split('/')
                .all(|segment| !segment.is_empty() && segment != "..");
        if valid {
            Ok(Self(path))
        } else {
            Err(IdError {
                kind: "relative path",
                value: path,
                pattern: RELATIVE_PATH_PATTERN,
            })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An identifier that does not match its pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdError {
    kind: &'static str,
    value: String,
    pattern: &'static str,
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid {} {:?}: expected {}",
            self.kind, self.value, self.pattern
        )
    }
}

impl std::error::Error for IdError {}

macro_rules! string_id {
    ($ty:ident, $pattern:expr, $description:expr) => {
        impl TryFrom<String> for $ty {
            type Error = IdError;
            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$ty> for String {
            fn from(id: $ty) -> String {
                id.0
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl JsonSchema for $ty {
            fn schema_name() -> Cow<'static, str> {
                stringify!($ty).into()
            }

            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "description": $description,
                    "type": "string",
                    "pattern": $pattern,
                })
            }
        }
    };
}

string_id!(
    TicketId,
    TICKET_ID_PATTERN,
    "A tracker's ticket identifier, safe as a git ref component and a file name."
);
string_id!(
    QuestionId,
    QUESTION_ID_PATTERN,
    "A question's identifier within a round: Q1, Q2, and so on."
);
string_id!(
    RelativePath,
    RELATIVE_PATH_PATTERN,
    "A path relative to the worktree: '/'-separated segments, none empty or '..', with no backslash or colon."
);
