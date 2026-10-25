//! Tickets: units of work read from the tracker, admitted by a human gesture.

use crate::pipeline::Pipeline;
use crate::state::TicketState;
use crate::vocab::{Priority, Variant};

/// What the core knows of a ticket. `K` is the ticket's identifier; the core
/// needs only its identity, and its format is the tracker's and the
/// contracts' concern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket<K> {
    pub id: K,
    pub priority: Priority,
    /// When the tracker created the ticket, in milliseconds since the Unix
    /// epoch: among tickets of equal priority, the first created goes first.
    pub created_ms: i64,
    pub admission: Admission,
    /// Whether the ticket has a decider (architecture, section 4).
    pub has_decider: bool,
    /// The hash of the ticket's current text, which the runner computes
    /// (architecture, section 5, "Intake"); the core only compares it.
    pub text_hash: String,
    /// The text hash its intake record holds, or `None` without a record.
    pub intake_hash: Option<String>,
    /// The tickets that block this one: the tracker's relations and the
    /// identifiers of its description's "Blocked by" line ([`text_blockers`]),
    /// each with the state the runner read for it. An identifier the tracker
    /// answered it does not know is [`BlockerState::Unknown`]; the ticket's own
    /// identifier is ignored by the ready set (architecture, section 5,
    /// "Blockers"). The runner passes identifiers as the tracker writes them,
    /// so the ticket's own compares equal.
    pub blocked_by: Vec<Blocker<K>>,
    /// The pipeline variant: picked at intake, or forced by a label.
    pub variant: Variant,
    /// The state its ticket ref keeps; [`TicketState::admitted`] for a ticket
    /// without a ref.
    pub state: TicketState,
    /// Whether Owlshift still follows the pull request it delivered for the
    /// ticket.
    pub delivery_followed: bool,
}

impl<K> Ticket<K> {
    /// The pipeline of the ticket's variant.
    pub const fn pipeline(&self) -> Pipeline {
        Pipeline::new(self.variant)
    }
}

/// Whether a person put the ticket in the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Admission {
    /// No admission gesture from a person the project lists.
    NotAdmitted,
    /// Admitted by delegation, a label or a state, from a listed person.
    Admitted,
    /// An exclusion label keeps it out for good, whatever else it carries.
    Excluded,
}

/// A ticket that blocks another.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Blocker<K> {
    pub id: K,
    /// What the tracker shows of it.
    pub tracker: BlockerState,
    /// Whether its ticket ref records its pull request merged.
    pub merged: bool,
}

impl<K> Blocker<K> {
    /// Whether it releases the ticket it blocks: the tracker shows it
    /// completed, or its pull request was merged. A canceled blocker never
    /// does (architecture, section 5, "Blockers").
    pub const fn released(&self) -> bool {
        self.merged || matches!(self.tracker, BlockerState::Completed)
    }
}

/// A blocker's state on the tracker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockerState {
    /// Not finished: on Linear, a state of type `triage`, `backlog`,
    /// `unstarted` or `started`.
    Open,
    /// On Linear, a state of type `completed`; on the Markdown tracker, the
    /// stage `Done`.
    Completed,
    /// Dropped: on Linear, a state of type `canceled` or `duplicate`. The
    /// ticket it blocks waits until a person removes the relation or the line.
    Canceled,
    /// The tracker answered that no such ticket exists: an identifier of the
    /// text convention that names no ticket, such as `UTF-8` or a typo. Only
    /// a definite "not found" is unknown; a read that failed otherwise is
    /// passed as open. The ticket it blocks waits until a person corrects the
    /// line.
    Unknown,
}

/// The blockers a description names by the text convention (architecture,
/// section 5, "Blockers"): on a line that starts, after list markers, heading
/// marks or emphasis, with "Blocked by" in any case, every `KEY-n` identifier,
/// each once, in the order they appear. "Blocked by" in the middle of a
/// sentence, or a heading whose identifiers are on the next line, names none.
pub fn text_blockers(description: &str) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    for line in description.lines() {
        let Some(rest) = after_blocked_by(line) else {
            continue;
        };
        for id in identifiers(rest) {
            if !found.contains(&id) {
                found.push(id);
            }
        }
    }
    found
}

/// The rest of a line that starts with "Blocked by", after its markers.
fn after_blocked_by(line: &str) -> Option<&str> {
    const PHRASE: &str = "blocked by";
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start();
        let stripped = trimmed
            .strip_prefix(['#', '*', '_'])
            .or_else(|| list_marker(trimmed));
        match stripped {
            Some(next) => rest = next,
            None => {
                rest = trimmed;
                break;
            }
        }
    }
    let head = rest.get(..PHRASE.len())?;
    if !head.eq_ignore_ascii_case(PHRASE) {
        return None;
    }
    let tail = &rest[PHRASE.len()..];
    match tail.chars().next() {
        Some(c) if c.is_ascii_alphanumeric() => None,
        _ => Some(tail),
    }
}

/// The text after a list marker (`-`, `+`, `1.` or `1)` and a space), if the
/// text starts with one. A `*` is stripped as emphasis already.
fn list_marker(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(['-', '+']).or_else(|| {
        let digits = text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            return None;
        }
        text[digits..].strip_prefix(['.', ')'])
    })?;
    rest.starts_with(char::is_whitespace).then_some(rest)
}

/// Every `KEY-n` identifier of a text: an uppercase ASCII letter, uppercase
/// letters or digits, a hyphen and digits, not inside a longer word.
fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    let mut start = 0;
    std::iter::from_fn(move || {
        while start < bytes.len() {
            let from = start;
            if !bytes[from].is_ascii_uppercase() || (from > 0 && word(bytes[from - 1])) {
                start += 1;
                continue;
            }
            let mut end = from + 1;
            while end < bytes.len()
                && (bytes[end].is_ascii_uppercase() || bytes[end].is_ascii_digit())
            {
                end += 1;
            }
            let key_end = end;
            if key_end < bytes.len() && bytes[key_end] == b'-' {
                end += 1;
                let digits = end;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end > digits && (end == bytes.len() || !word(bytes[end])) {
                    start = end;
                    return Some(&text[from..end]);
                }
            }
            start = key_end.max(from + 1);
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_phrasings_found_on_the_backlog_name_their_blockers() {
        let cases: [(&str, &[&str]); 13] = [
            ("Blocked by OWL-203.", &["OWL-203"]),
            ("Blocked by OWL-33 and OWL-122.", &["OWL-33", "OWL-122"]),
            ("Blocked by OWL-220 (same line).", &["OWL-220"]),
            (
                "Blocked by OWL-115 and by the verdict-folding follow-up.",
                &["OWL-115"],
            ),
            ("Blocked by the remaining P2 work.", &[]),
            ("## Blocked by\n\nOWL-98", &[]),
            ("Filed as follow-ups blocked by this issue (OWL-7).", &[]),
            ("- **Blocked by** OWL-1", &["OWL-1"]),
            ("1. blocked BY OWL-2, OWL-2", &["OWL-2"]),
            ("Why\r\nBlocked by: OWL-3\r\nOWL-4", &["OWL-3"]),
            ("Blocked byOWL-5", &[]),
            ("> Blocked by OWL-6", &[]),
            (
                "Blocked by OWL-8.\n\n_Blocked by_ OWL-9 and OWL-8",
                &["OWL-8", "OWL-9"],
            ),
        ];
        for (description, expected) in cases {
            assert_eq!(text_blockers(description), expected, "{description:?}");
        }
    }

    #[test]
    fn only_whole_key_n_identifiers_count() {
        assert_eq!(
            text_blockers("Blocked by owl-1, XOWL-2x, OWL-3a, OWL-, -OWL-4, (OWL-5), A1-7"),
            ["OWL-5", "A1-7"]
        );
    }

    #[test]
    fn only_a_completed_or_merged_blocker_releases() {
        let blocker = |tracker, merged| Blocker {
            id: "OWL-1",
            tracker,
            merged,
        };
        assert!(!blocker(BlockerState::Open, false).released());
        assert!(!blocker(BlockerState::Canceled, false).released());
        assert!(!blocker(BlockerState::Unknown, false).released());
        assert!(blocker(BlockerState::Completed, false).released());
        assert!(blocker(BlockerState::Open, true).released());
    }
}
