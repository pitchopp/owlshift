//! When a decider's reply counts (architecture, section 4, "No premature
//! resume"; scenario S2, step 3).
//!
//! A reply counts once the decider's latest comment has been left unedited
//! for the quiet window, so the answer check never reads an answer still
//! being written, or at once when that comment ends with `go`. Decided on
//! 2026-10-03 (OWL-127): `owlshift continue` applies it as `watch` will. The
//! caller measures how long the comment has been left alone; the core reads
//! no clock.

use std::time::Duration;

/// How long the decider's latest comment must be left unedited before it
/// counts when the project's policy does not set it
/// (`policy.quiet_window_minutes`, OWL-145): 10 minutes.
pub const DEFAULT_QUIET_WINDOW: Duration = Duration::from_secs(10 * 60);

/// Whether a reply counts now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The answer check may read it.
    Counts,
    /// The decider may still be writing: wait until the window has passed.
    Settling,
}

/// Whether a reply whose latest comment was left unedited for `quiet_for`
/// counts, `go` saying whether that comment ends with `go`
/// ([`ends_with_go`]). It counts once `quiet_for` reaches `window`.
pub fn counts(quiet_for: Duration, go: bool, window: Duration) -> Reply {
    if go || quiet_for >= window {
        Reply::Counts
    } else {
        Reply::Settling
    }
}

/// Whether a comment ends with the word `go`, in any ASCII case: its last
/// word, once whitespace and punctuation are trimmed from its end and from
/// around that word. `Go.`, `let's go!` and `` `go` `` end with it; `ago`,
/// `no-go` and `go on` do not.
pub fn ends_with_go(body: &str) -> bool {
    let trimmed = body.trim_end_matches(|c: char| c.is_whitespace() || c.is_ascii_punctuation());
    trimmed
        .split_whitespace()
        .next_back()
        .map(|word| word.trim_matches(|c: char| c.is_ascii_punctuation()))
        .is_some_and(|word| word.eq_ignore_ascii_case("go"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_counts_once_the_window_has_passed_or_with_go() {
        let minute = Duration::from_secs(60);
        assert_eq!(
            counts(9 * minute, false, DEFAULT_QUIET_WINDOW),
            Reply::Settling
        );
        assert_eq!(
            counts(DEFAULT_QUIET_WINDOW, false, DEFAULT_QUIET_WINDOW),
            Reply::Counts
        );
        assert_eq!(
            counts(Duration::ZERO, true, DEFAULT_QUIET_WINDOW),
            Reply::Counts
        );
    }

    #[test]
    fn only_a_last_word_go_ends_with_go() {
        for body in [
            "go",
            "Q1: English.\nGO",
            "Q1: yes. Go.",
            "let's go!",
            "`go`",
            "**go**\n\n",
        ] {
            assert!(ends_with_go(body), "{body:?}");
        }
        for body in [
            "",
            "ago",
            "Q1: no-go",
            "go on",
            "go\nQ2: later",
            "...",
            "gogo",
        ] {
            assert!(!ends_with_go(body), "{body:?}");
        }
    }
}
