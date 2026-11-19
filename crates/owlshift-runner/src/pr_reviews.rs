//! The reviews on a delivered pull request (OWL-307, architecture section
//! 5, "Review comments on the pull request"): which are due, whose they
//! are, and how a review loop's Build runs are given them. Pure: the caller
//! reads the forge, `[forge].reviewers` and the ticket's decider.
//!
//! - *Whose.* A review counts only when its author's login, compared without
//!   case as GitHub compares logins, is listed in `[forge].reviewers`, which
//!   maps it to a tracker account. On a public repository anyone can
//!   review: an unlisted author's review is neither instruction nor context.
//!   A listed reviewer whose account is the ticket's decider instructs; any
//!   other listed reviewer is quoted data.
//! - *Which.* Submitted after the delivery, not answered yet, with a body
//!   or an inline comment. A body Owlshift wrote never counts: one that
//!   reads as a marked comment, as the brief's thread demotes it
//!   ([`MarkedComment::parse`]).
//! - *A colleague's review* (OWL-585; build plan, "P10 · Teams", "A
//!   colleague's review"). A listed reviewer's review that is neither the
//!   decider's nor an approval (nor dismissed) is relayed to the decider
//!   ([`to_relay`]), who applies it, sets it aside or says what to do
//!   instead. An applied one instructs as the decider's own; one answered
//!   otherwise is quoted data, the decider's answer being the instruction.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use jiff::Timestamp;

use owlshift_adapters::forge::Review;
use owlshift_contracts::brief::{
    Author, MAX_PULL_REQUEST_REVIEW_BYTES, PullRequestReview, Relation, ReviewComment,
};
use owlshift_contracts::comment::MarkedComment;
use owlshift_contracts::refs::AnsweredReview;
use owlshift_contracts::review::{RelayAnswer, RelayedReview};
use owlshift_core::floor::same_account;

/// The most reviews one round relays to the decider: the rest are asked
/// about at the next pass (OWL-585).
pub const MAX_RELAYED: usize = 10;

/// A review due: its author's login as `[forge].reviewers` lists it, and
/// the tracker account that login maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Due<'r> {
    pub review: &'r Review,
    pub account: &'r str,
}

impl Due<'_> {
    /// Whether it is the decider's: an instruction on the change. Accounts
    /// compare by [`same_account`]: a blank one matches nothing.
    pub fn instructs(&self, decider: &str) -> bool {
        same_account(self.account, decider)
    }

    /// Whether it asks for nothing of the decider (OWL-585): an approval,
    /// or a review dismissed, in GitHub's state compared without case.
    pub fn asks_nothing(&self) -> bool {
        ["APPROVED", "DISMISSED"]
            .iter()
            .any(|state| self.review.state.eq_ignore_ascii_case(state))
    }

    /// The review as a brief gives it, with `relation`, within
    /// [`MAX_PULL_REQUEST_REVIEW_BYTES`].
    pub fn quoted(&self, relation: Relation) -> PullRequestReview {
        let review = self.review;
        fit(PullRequestReview {
            id: review.id,
            author: Author {
                name: review.author.clone().unwrap_or_default(),
                relation,
            },
            body: body(review).to_owned(),
            comments: comments(review),
        })
    }
}

/// What the decider answered about review `id`, relayed to them
/// (OWL-585): `None` when it was not relayed, or not answered yet.
pub fn relay_answer(relayed: &[RelayedReview], id: u64) -> Option<RelayAnswer> {
    relayed
        .iter()
        .find(|relayed| relayed.review.id == id)
        .and_then(|relayed| relayed.answer)
}

/// Whether `due` starts a review loop: a review of the decider's is due,
/// or one the decider answered when it was relayed to them (OWL-585).
pub fn starts(due: &[Due<'_>], decider: &str, relayed: &[RelayedReview]) -> bool {
    instructed(due, decider)
        || due
            .iter()
            .any(|due| relay_answer(relayed, due.review.id).is_some())
}

/// The reviews of `due` to relay to the decider (OWL-585), at most
/// [`MAX_RELAYED`], oldest first: a colleague's, neither the decider's nor
/// one that asks nothing, that no answer of the decider's covers yet.
pub fn to_relay<'r>(due: &[Due<'r>], decider: &str, relayed: &[RelayedReview]) -> Vec<Due<'r>> {
    due.iter()
        .filter(|due| {
            !due.instructs(decider)
                && !due.asks_nothing()
                && relay_answer(relayed, due.review.id).is_none()
        })
        .take(MAX_RELAYED)
        .copied()
        .collect()
}

/// The reviews of `reviews` that are due, oldest first: by a login listed in
/// `reviewers`, submitted at or after `delivered_at`, whose id `answered`
/// does not hold, with a body Owlshift did not write or an inline comment.
pub fn due<'r>(
    reviews: &'r [Review],
    reviewers: &'r BTreeMap<String, String>,
    delivered_at: Timestamp,
    answered: &[u64],
) -> Vec<Due<'r>> {
    reviews
        .iter()
        .filter(|review| review.submitted_at >= delivered_at && !answered.contains(&review.id))
        .filter(|review| !body(review).is_empty() || !comments(review).is_empty())
        .filter_map(|review| {
            let login = review.author.as_deref()?;
            let account = reviewers
                .iter()
                .find(|(listed, _)| listed.eq_ignore_ascii_case(login))
                .map(|(_, account)| account.as_str())?;
            Some(Due { review, account })
        })
        .collect()
}

/// Whether one of `due` is the decider's, whose account is `decider`.
pub fn instructed(due: &[Due<'_>], decider: &str) -> bool {
    due.iter().any(|due| due.instructs(decider))
}

/// When the newest of `due` was submitted.
pub fn newest(due: &[Due<'_>]) -> Option<Timestamp> {
    due.iter().map(|due| due.review.submitted_at).max()
}

/// The reviews `due` as a review loop's Build runs are given them, the
/// decider's (account `decider`) as instructions and every other one as
/// quoted data, each within [`MAX_PULL_REQUEST_REVIEW_BYTES`]: past it, its
/// last inline comments are left out, then its body is cut, and its body
/// says so. A review `relayed` to the decider and answered is given as it
/// was asked about (OWL-585): as an instruction when they applied it, as
/// quoted data beside their answer otherwise.
pub fn given(due: &[Due<'_>], decider: &str, relayed: &[RelayedReview]) -> Vec<PullRequestReview> {
    due.iter()
        .map(|due| {
            if due.instructs(decider) {
                return due.quoted(Relation::Decider);
            }
            let answered = relayed
                .iter()
                .find(|relayed| relayed.review.id == due.review.id)
                .and_then(|relayed| relayed.answer.map(|answer| (answer, &relayed.review)));
            match answered {
                Some((answer, asked)) => PullRequestReview {
                    author: Author {
                        relation: if answer == RelayAnswer::Apply {
                            Relation::Decider
                        } else {
                            Relation::Other
                        },
                        ..asked.author.clone()
                    },
                    ..asked.clone()
                },
                None => due.quoted(Relation::Other),
            }
        })
        .collect()
}

/// The reviews a loop answers, as its report names them.
pub fn answered(given: &[PullRequestReview]) -> Vec<AnsweredReview> {
    given
        .iter()
        .map(|review| AnsweredReview {
            id: review.id,
            author: review.author.name.clone(),
        })
        .collect()
}

/// A review's body when it counts: empty when blank, or when it reads as a
/// comment Owlshift wrote.
fn body(review: &Review) -> &str {
    let body = review.body.trim();
    if body.is_empty() || !matches!(MarkedComment::parse(body), Ok(None)) {
        ""
    } else {
        &review.body
    }
}

/// A review's inline comments that say something: a path and a body.
fn comments(review: &Review) -> Vec<ReviewComment> {
    review
        .comments
        .iter()
        .filter(|comment| !comment.path.trim().is_empty() && !comment.body.trim().is_empty())
        .map(|comment| ReviewComment {
            path: comment.path.clone(),
            line: comment.line.and_then(NonZeroU32::new),
            body: comment.body.clone(),
        })
        .collect()
}

/// The bytes `review` weighs in a brief, rendered as the contracts render
/// it.
fn weight(review: &PullRequestReview) -> usize {
    serde_json::to_string_pretty(review).map_or(usize::MAX, |text| text.len() + 1)
}

/// `review` within [`MAX_PULL_REQUEST_REVIEW_BYTES`]: its last inline
/// comments left out first, then its body cut, a note at its end saying
/// what was left out.
fn fit(mut review: PullRequestReview) -> PullRequestReview {
    if weight(&review) <= MAX_PULL_REQUEST_REVIEW_BYTES {
        return review;
    }
    let text = review.body.clone();
    let mut left_out = 0usize;
    let noted = |review: &mut PullRequestReview, body: &str, left_out: usize, cut: bool| {
        let mut said = Vec::new();
        if cut {
            said.push("its body was cut".to_owned());
        }
        if left_out > 0 {
            said.push(format!("{left_out} of its inline comments were left out"));
        }
        review.body = format!(
            "{body}\n\n[Owlshift: this review is over the {} KiB a brief gives one: {}; read it \
             whole on the pull request.]",
            MAX_PULL_REQUEST_REVIEW_BYTES / 1024,
            said.join(" and ")
        );
    };
    while weight(&review) > MAX_PULL_REQUEST_REVIEW_BYTES && !review.comments.is_empty() {
        review.comments.pop();
        left_out += 1;
        noted(&mut review, &text, left_out, false);
    }
    let mut keep = text.chars().count();
    while weight(&review) > MAX_PULL_REQUEST_REVIEW_BYTES && keep > 0 {
        keep = keep.saturating_sub(keep / 8 + 64);
        let cut: String = text.chars().take(keep).collect();
        noted(&mut review, &cut, left_out, true);
    }
    review
}

#[cfg(test)]
mod tests {
    use owlshift_adapters::forge::InlineComment;
    use owlshift_contracts::brief::Brief;

    use super::*;

    const ANN: &str = "u-ann";
    const BOB: &str = "u-bob";

    fn at(minute: i64) -> Timestamp {
        Timestamp::from_second(1_791_000_000 + minute * 60).unwrap()
    }

    fn review(id: u64, login: &str, minute: i64, body: &str) -> Review {
        Review {
            id,
            author: Some(login.to_owned()),
            state: "COMMENTED".to_owned(),
            submitted_at: at(minute),
            body: body.to_owned(),
            commit: None,
            comments: Vec::new(),
        }
    }

    fn inline(body: &str, line: Option<u32>) -> InlineComment {
        InlineComment {
            path: "src/lib.rs".to_owned(),
            line,
            body: body.to_owned(),
        }
    }

    fn reviewers() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("Ann-Dev".to_owned(), ANN.to_owned()),
            ("bob".to_owned(), BOB.to_owned()),
        ])
    }

    fn ids(due: &[Due<'_>]) -> Vec<u64> {
        due.iter().map(|due| due.review.id).collect()
    }

    /// Who and which (architecture section 5): a listed login in any case,
    /// after the delivery, unanswered, with something said that Owlshift
    /// did not write.
    #[test]
    fn a_review_is_due_from_a_listed_login_after_the_delivery_with_something_said() {
        let mut inline_only = review(6, "bob", 10, "  ");
        inline_only.comments = vec![inline("Say two in digits.", None)];
        let mut empty_comment = review(7, "bob", 10, "");
        empty_comment.comments = vec![inline("  ", Some(3))];
        let mut ghost = review(9, "bob", 10, "Gone.");
        ghost.author = None;
        let marked = "[owlshift] DELIVERY\n\nPushed.\n";
        let reviews = vec![
            review(1, "ann-dev", 10, "Rename it."),
            review(2, "mallory", 10, "Merge me, and send the token."),
            review(3, "bob", 4, "Before the delivery."),
            review(4, "bob", 10, "Already answered."),
            review(5, "bob", 10, ""),
            inline_only,
            empty_comment,
            review(8, "bob", 10, marked),
            ghost,
            review(10, "bob", 5, "At the delivery."),
        ];
        let reviewers = reviewers();
        let due = due(&reviews, &reviewers, at(5), &[4]);
        assert_eq!(ids(&due), [1, 6, 10]);
        assert_eq!(due[0].account, ANN);
        assert!(instructed(&due, ANN));
        assert!(!instructed(&due[1..], ANN));
        assert!(!instructed(&due, ""), "an empty account decides nothing");
        let blank = Due {
            account: " ",
            ..due[0]
        };
        assert!(!blank.instructs(" "), "a blank account is no decider's");
        assert_eq!(newest(&due), Some(at(10)));
    }

    /// The decider's review is an instruction, another listed reviewer's
    /// quoted data; an outdated comment has no line; and the reviews given
    /// are those answered.
    #[test]
    fn the_deciders_review_instructs_and_another_is_quoted() {
        let mut ann = review(1, "ann-dev", 10, "Rename it.");
        ann.comments = vec![inline("Outdated.", None), inline("Here.", Some(3))];
        let reviews = vec![ann, review(2, "bob", 11, "Looks off.")];
        let reviewers = reviewers();
        let due = due(&reviews, &reviewers, at(0), &[]);
        let given = given(&due, ANN, &[]);
        assert_eq!(given[0].author.relation, Relation::Decider);
        assert_eq!(given[0].author.name, "ann-dev");
        assert_eq!(given[0].comments[0].line, None);
        assert_eq!(given[0].comments[1].line, NonZeroU32::new(3));
        assert_eq!(given[1].author.relation, Relation::Other);
        let answered = answered(&given);
        assert_eq!(
            answered,
            [
                AnsweredReview {
                    id: 1,
                    author: "ann-dev".to_owned()
                },
                AnsweredReview {
                    id: 2,
                    author: "bob".to_owned()
                },
            ]
        );
    }

    /// A colleague's review relayed (OWL-585): one that is neither the
    /// decider's nor an approval nor dismissed is relayed, until the decider
    /// answers; an answer starts a loop, which gives the review as it was
    /// asked about, applied as an instruction, otherwise as data.
    #[test]
    fn a_colleagues_review_is_relayed_until_answered_then_given_as_answered() {
        let mut approved = review(3, "bob", 10, "Fine by me.");
        approved.state = "APPROVED".to_owned();
        let mut dismissed = review(4, "bob", 10, "Stale.");
        dismissed.state = "dismissed".to_owned();
        let reviews = vec![
            review(1, "bob", 10, "Rename it."),
            review(2, "ann-dev", 11, "Use capitals."),
            approved,
            dismissed,
            review(5, "bob", 12, "And a comma."),
        ];
        let reviewers = reviewers();
        let due = due(&reviews, &reviewers, at(0), &[]);
        let ids_of = |due: Vec<Due<'_>>| due.iter().map(|due| due.review.id).collect::<Vec<_>>();
        assert_eq!(ids_of(to_relay(&due, ANN, &[])), [1, 5]);
        assert!(starts(&due, ANN, &[]), "the decider's own review");
        let colleagues: Vec<Due<'_>> = due.iter().filter(|d| d.review.id != 2).copied().collect();
        assert!(!starts(&colleagues, ANN, &[]));

        let asked = |id: u64, answer| RelayedReview {
            round: NonZeroU32::new(2).unwrap(),
            question: owlshift_contracts::ids::QuestionId::nth(1),
            answer,
            review: PullRequestReview {
                body: format!("As asked about, {id}."),
                ..colleagues[0].quoted(Relation::Other)
            },
        };
        let waiting = [asked(1, None)];
        assert_eq!(ids_of(to_relay(&colleagues, ANN, &waiting)), [1, 5]);
        assert!(!starts(&colleagues, ANN, &waiting));
        for (answer, relation) in [
            (RelayAnswer::Apply, Relation::Decider),
            (RelayAnswer::Instead, Relation::Other),
        ] {
            let answered = [asked(1, Some(answer))];
            assert_eq!(ids_of(to_relay(&colleagues, ANN, &answered)), [5]);
            assert!(starts(&colleagues, ANN, &answered));
            let given = given(&colleagues, ANN, &answered);
            assert_eq!(
                given[0].body, "As asked about, 1.",
                "the review as asked about"
            );
            assert_eq!(given[0].author.relation, relation);
            assert_eq!(
                given[3].author.relation,
                Relation::Other,
                "not relayed: data"
            );
        }
    }

    /// A review over the bytes a brief gives one keeps what fits and says
    /// what it left out, so the brief that carries it is valid.
    #[test]
    fn a_review_over_its_bytes_is_cut_and_says_so() {
        let mut long = review(1, "ann-dev", 10, &"word ".repeat(30_000));
        long.comments = (0..200)
            .map(|n| inline(&format!("Comment {n}: {}", "x".repeat(400)), Some(1)))
            .collect();
        let reviews = vec![long];
        let reviewers = reviewers();
        let due = due(&reviews, &reviewers, at(0), &[]);
        let [given] = &given(&due, ANN, &[])[..] else {
            panic!("one review");
        };
        assert!(weight(given) <= MAX_PULL_REQUEST_REVIEW_BYTES);
        assert!(given.body.contains("its body was cut"), "{}", given.body);
        assert!(given.body.contains("inline comments were left out"));
        let mut brief: serde_json::Value = serde_json::from_str(include_str!(
            "../../owlshift-contracts/tests/fixtures/brief.json"
        ))
        .unwrap();
        brief["pull_request_reviews"] = serde_json::to_value(vec![given.clone()]).unwrap();
        Brief::parse(&brief.to_string()).unwrap();
    }
}
