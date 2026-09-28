//! Marked comments: the comments the runner posts on a ticket.
//!
//! A marked comment starts with a header line such as
//! `[owlshift] QUESTIONS · round 2` and may end with a machine-readable footer,
//! `<!-- owlshift:{…} -->`, on its last non-blank line. A tracker that shows
//! HTML comments may lose or display the footer; the header alone still
//! identifies the comment.

use std::fmt;
use std::num::NonZeroU32;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::format::{self, ContractError, FOOTER_FORMAT, Format};
use crate::ids::TicketId;

/// The prefix of a header line.
pub const HEADER_PREFIX: &str = "[owlshift] ";
/// The opening of a footer.
pub const FOOTER_OPEN: &str = "<!-- owlshift:";
/// The closing of a footer.
pub const FOOTER_CLOSE: &str = " -->";

const HEADER: &str = "comment header";
const FOOTER: &str = "comment footer";
const ROUND_SEPARATOR: &str = " · round ";

/// What a marked comment is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING-KEBAB-CASE")]
pub enum MarkerKind {
    /// A round of questions for the decider.
    Questions,
    /// The questions of a round left unanswered, asked again.
    ReAsk,
    /// What was understood from the answers; the ticket resumes.
    Resume,
    /// A decision taken without a human, reversible.
    Decision,
    /// The ticket is parked: why, and what would restart it.
    Parked,
    /// The delivery report.
    Delivery,
    /// An answer to a counter-question in the thread.
    Reply,
}

impl MarkerKind {
    const ALL: [Self; 7] = [
        Self::Questions,
        Self::ReAsk,
        Self::Resume,
        Self::Decision,
        Self::Parked,
        Self::Delivery,
        Self::Reply,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Questions => "QUESTIONS",
            Self::ReAsk => "RE-ASK",
            Self::Resume => "RESUME",
            Self::Decision => "DECISION",
            Self::Parked => "PARKED",
            Self::Delivery => "DELIVERY",
            Self::Reply => "REPLY",
        }
    }

    /// Whether a comment of this kind names its question round.
    pub fn has_round(self) -> bool {
        matches!(self, Self::Questions | Self::ReAsk | Self::Resume)
    }

    fn check_round(
        self,
        contract: &'static str,
        round: Option<NonZeroU32>,
    ) -> Result<(), ContractError> {
        match (self.has_round(), round) {
            (true, None) => Err(ContractError::invalid(
                contract,
                format!("{self} needs a round"),
            )),
            (false, Some(_)) => Err(ContractError::invalid(
                contract,
                format!("{self} takes no round"),
            )),
            _ => Ok(()),
        }
    }
}

impl fmt::Display for MarkerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The first line of a marked comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: MarkerKind,
    /// The question round, for QUESTIONS, RE-ASK and RESUME only.
    pub round: Option<NonZeroU32>,
}

impl Header {
    /// Renders the header line, without a line break.
    pub fn render(&self) -> String {
        match self.round {
            Some(round) => format!("{HEADER_PREFIX}{}{ROUND_SEPARATOR}{round}", self.kind),
            None => format!("{HEADER_PREFIX}{}", self.kind),
        }
    }

    /// Parses a header line; a trailing line break or spaces are ignored.
    pub fn parse(line: &str) -> Result<Self, ContractError> {
        let line = line.trim_end();
        let rest = line
            .strip_prefix(HEADER_PREFIX)
            .ok_or_else(|| ContractError::invalid(HEADER, format!("{line:?} is not a header")))?;
        let (kind, round) = match rest.split_once(ROUND_SEPARATOR) {
            Some((kind, round)) => {
                let round = if round.bytes().all(|b| b.is_ascii_digit()) && !round.starts_with('0')
                {
                    round.parse::<NonZeroU32>().ok()
                } else {
                    None
                };
                let round = round.ok_or_else(|| {
                    ContractError::invalid(HEADER, format!("{line:?} has an invalid round"))
                })?;
                (kind, Some(round))
            }
            None => (rest, None),
        };
        let kind = MarkerKind::ALL
            .into_iter()
            .find(|k| k.as_str() == kind)
            .ok_or_else(|| {
                ContractError::invalid(HEADER, format!("{line:?} has an unknown kind"))
            })?;
        kind.check_round(HEADER, round)?;
        Ok(Self { kind, round })
    }
}

/// The machine-readable footer of a marked comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift comment footer")]
pub struct Footer {
    pub format: Format<FOOTER_FORMAT>,
    pub kind: MarkerKind,
    pub ticket: TicketId,
    /// The question round, for QUESTIONS, RE-ASK and RESUME only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub round: Option<NonZeroU32>,
    /// The run that produced the comment, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

impl Footer {
    /// Renders the footer line, without a line break.
    ///
    /// `>` is escaped inside the JSON so no value can close the HTML comment.
    pub fn render(&self) -> String {
        let json = serde_json::to_string(self)
            .expect("contract types always serialize to JSON")
            .replace('>', "\\u003e");
        format!("{FOOTER_OPEN}{json}{FOOTER_CLOSE}")
    }

    /// Parses the footer's JSON payload.
    pub fn parse_payload(json: &str) -> Result<Self, ContractError> {
        let footer: Self = format::parse_json(FOOTER, FOOTER_FORMAT, json)?;
        footer.kind.check_round(FOOTER, footer.round)?;
        Ok(footer)
    }

    /// Finds the footer of a comment body: its last non-blank line, if that
    /// line opens a footer. A footer elsewhere in the body, such as one quoted
    /// from another comment, is ignored.
    pub fn find(body: &str) -> Result<Option<Self>, ContractError> {
        let Some(last) = body.lines().map(str::trim).rfind(|l| !l.is_empty()) else {
            return Ok(None);
        };
        let Some(rest) = last.strip_prefix(FOOTER_OPEN) else {
            return Ok(None);
        };
        let json = rest
            .strip_suffix(FOOTER_CLOSE.trim_start())
            .ok_or_else(|| ContractError::invalid(FOOTER, "the footer is not closed"))?;
        Self::parse_payload(json.trim()).map(Some)
    }
}

/// A marked comment: its header and, where the tracker kept it, its footer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkedComment {
    pub header: Header,
    pub footer: Option<Footer>,
}

impl MarkedComment {
    /// Reads a comment body. Returns `None` for a comment that is not marked
    /// (no header on its first line and no footer), and an error for a
    /// malformed marker or a footer that disagrees with the header.
    pub fn parse(body: &str) -> Result<Option<Self>, ContractError> {
        let first = body.lines().next().unwrap_or_default();
        let footer = Footer::find(body)?;
        if !first.starts_with(HEADER_PREFIX.trim_end()) {
            return match footer {
                None => Ok(None),
                Some(_) => Err(ContractError::invalid(
                    HEADER,
                    "the comment has a footer but no header",
                )),
            };
        }
        let header = Header::parse(first)?;
        if let Some(footer) = &footer
            && (footer.kind != header.kind || footer.round != header.round)
        {
            return Err(ContractError::invalid(
                FOOTER,
                "the footer disagrees with the header",
            ));
        }
        Ok(Some(Self { header, footer }))
    }
}
