//! When a decider's reply counts (architecture, section 4, "No premature
//! resume"; scenario S2, step 3).
//!
//! A reply counts once the decider's latest comment has been left unedited
//! for the quiet window, so the answer check never reads an answer still
//! being written, or at once when that comment ends with `go`. Decided on
//! 2026-10-03 (OWL-127): `owlshift continue` applies it as `watch` will. The
//! caller measures how long the comment has been left alone; the core reads
//! no clock.
//!
//! Which decisions a reply under a DECISION comment names (OWL-351): that
//! comment holds a resolver run's decisions, D1..Dn, and a reply reverses
//! those it names ([`named_decisions`]).

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

/// The numbers of the decisions `body` names among `count`, ascending, each
/// once (OWL-351): a word `Dn`, or `Qn` taken as `Dn`, in any ASCII case,
/// anywhere in it, with `n` from 1 to `count`. A word is a run of ASCII
/// letters and digits, so `**D2**` and `D2:` name D2 and `AD2` or `D2a` do
/// not. Empty when it names none: the reply then concerns every decision.
pub fn named_decisions(body: &str, count: usize) -> Vec<usize> {
    let mut named: Vec<usize> = body
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|word| {
            let digits = word.strip_prefix(['D', 'd', 'Q', 'q']).filter(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            })?;
            digits.parse::<usize>().ok()
        })
        .filter(|n| (1..=count).contains(n))
        .collect();
    named.sort_unstable();
    named.dedup();
    named
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_names_the_decisions_it_numbers() {
        for (body, named) in [
            ("D2: keep the README as it is", &[2][..]),
            ("**d2** no", &[2]),
            ("Q3: B instead", &[3]),
            ("I disagree with D1 and D3, not D1 again", &[1, 3]),
            ("No, use English.", &[]),
            ("D12 and D0", &[]),
            ("AD2, D2a, D-2", &[]),
            ("D02", &[2]),
        ] {
            assert_eq!(named_decisions(body, 3), named, "{body:?}");
        }
    }

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
