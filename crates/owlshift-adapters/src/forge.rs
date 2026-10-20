//! Forges: where the ticket's branch is pushed, its pull request opened, and
//! the checks and merge state of that pull request read.
//!
//! Only the runner holds forge credentials, and no forge operation here
//! merges: a human merges (architecture section 8). Pushing is plain git
//! through the user's own git credentials ([`push`]); the rest is each
//! forge's API ([`github`]). There is no `Forge` trait yet: it is extracted
//! at the second forge adapter, like the tracker's.
//!
//! A pull request is announced green only on its complete check set: every
//! check of the exact commit the runner pushed, read to the last page, with
//! the pull request open and free of conflicts ([`CheckSet::verdict`]).

pub mod github;
pub mod github_app;
pub mod push;

use std::fmt;

use jiff::Timestamp;
use owlshift_contracts::ids::is_repository_part;

/// A row of the forge line in architecture section 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Push the ticket's branch.
    PushBranch,
    /// Push Owlshift's own refs: claims and ticket state.
    PushRef,
    /// Open a pull request.
    OpenPullRequest,
    /// Read the complete check set of a pull request.
    ReadChecks,
    /// Read review comments.
    ReadReviewComments,
    /// Read whether a pull request can merge.
    ReadMergeState,
}

impl Capability {
    /// Every row, in the table's order.
    pub const ALL: [Self; 6] = [
        Self::OpenPullRequest,
        Self::ReadChecks,
        Self::ReadReviewComments,
        Self::PushBranch,
        Self::PushRef,
        Self::ReadMergeState,
    ];

    /// A short description, for reports such as `owlshift doctor`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::PushBranch => "push branches",
            Self::PushRef => "push custom refs",
            Self::OpenPullRequest => "open a pull request",
            Self::ReadChecks => "read the complete check set",
            Self::ReadReviewComments => "read review comments",
            Self::ReadMergeState => "read merge state",
        }
    }
}

/// A repository on a forge: `owner/name`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Repo {
    owner: String,
    name: String,
}

impl Repo {
    /// Parses `owner/name`.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let invalid = || {
            invalid(format!(
                "{text:?} is not a repository of the form owner/name"
            ))
        };
        let (owner, name) = text.split_once('/').ok_or_else(invalid)?;
        if !is_repository_part(owner) || !is_repository_part(name) {
            return Err(invalid());
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// The repository a GitHub remote URL points to: `git@github.com:o/r.git`,
    /// `ssh://git@github.com/o/r.git` or `https://github.com/o/r.git`, with or
    /// without the `.git` suffix. Any other host is refused. In an https URL
    /// the user info is read inside the authority only, so an `@` in the
    /// path (`https://example.invalid/@github.com/o/r`) does not make another
    /// host pass for github.com.
    pub fn from_github_remote(url: &str) -> Result<Self, Error> {
        let path = url
            .strip_prefix("git@github.com:")
            .or_else(|| url.strip_prefix("ssh://git@github.com/"))
            .or_else(|| {
                let (authority, path) = url.strip_prefix("https://")?.split_once('/')?;
                let host = authority
                    .rsplit_once('@')
                    .map_or(authority, |(_, host)| host);
                (host == "github.com").then_some(path)
            })
            .ok_or_else(|| invalid("the remote is not a github.com repository"))?;
        let path = path.trim_end_matches('/');
        Self::parse(path.strip_suffix(".git").unwrap_or(path))
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for Repo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// A branch name, checked against git's rules for ref names
/// (`git check-ref-format --branch`), so it is safe in a refspec.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Branch(String);

impl Branch {
    pub fn new(name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        let bad_component = name
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"));
        let bad_char = name.chars().any(|c| {
            c.is_ascii_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        });
        if name.is_empty()
            || name.starts_with('-')
            || name.ends_with('.')
            || name == "@"
            || name.contains("..")
            || name.contains("@{")
            || bad_component
            || bad_char
        {
            return Err(invalid(format!("{name:?} is not a valid branch name")));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Branch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A full commit id: 40 hexadecimal digits (SHA-1) or 64 (SHA-256).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CommitId(String);

impl CommitId {
    pub fn new(id: impl Into<String>) -> Result<Self, Error> {
        let id = id.into();
        let hex = id.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'));
        if !(hex && matches!(id.len(), 40 | 64)) {
            return Err(invalid(format!("{id:?} is not a full commit id")));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A pull request as the forge answered it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    /// The commit its head branch pointed to when read.
    pub head: CommitId,
    pub state: PrState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// One check of a commit: a check run or a commit status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub kind: CheckKind,
    /// The check run's name, or the commit status's context.
    pub name: String,
    pub state: CheckState,
    /// The forge's own value behind `state`, such as `SUCCESS`, `TIMED_OUT`
    /// or `IN_PROGRESS`, for reports.
    pub raw: String,
    /// Whether branch protection requires it for this pull request.
    pub required: bool,
    /// Where its details are, when the forge gives one.
    pub url: Option<String>,
    /// The check run's id, which for GitHub Actions is its job's: given by
    /// watch's read (OWL-112), absent from the delivery's.
    pub id: Option<u64>,
    /// A commit status's description: given by watch's read (OWL-112).
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckKind {
    /// A check run, with the app that created it and, for GitHub Actions,
    /// its workflow.
    CheckRun {
        app: Option<String>,
        workflow: Option<String>,
    },
    /// A commit status.
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Passed,
    /// Not finished yet: queued, running, or expected but not reported.
    Pending,
    /// Anything that is neither passed nor pending, including a value the
    /// adapter does not know: it reaches a human rather than passing.
    Failed,
}

/// Whether the pull request merges without conflicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    /// Not computed yet, or a value the adapter does not know.
    Unknown,
}

/// Every check of a pull request's head commit, with its merge state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckSet {
    pub pull_request: u64,
    /// The commit the checks belong to: the one the caller expected, or, for
    /// watch's read, the head read.
    pub head: CommitId,
    pub state: PrState,
    pub mergeable: Mergeable,
    /// The forge's own merge state (GitHub's `mergeStateStatus`, such as
    /// `CLEAN`, `BLOCKED` or `BEHIND`). It depends on reviews and branch
    /// protection that Owlshift cannot satisfy by itself, so it is reported,
    /// not part of the verdict.
    pub merge_state: String,
    /// Every check, in the forge's order; never a partial list.
    pub checks: Vec<Check>,
}

/// What GitHub said of the query's cost (`rateLimit`, OWL-209): the points
/// it cost, summed over its pages, and the budget left after the last one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub cost: u64,
    pub remaining: u64,
    pub resets_at: Timestamp,
}

/// Watch's read of a pull request it follows: its check set at the head
/// read, whoever pushed it, and the read's cost when GitHub reported it. A
/// pull request that is not open has no checks listed: the read ends the
/// following before any check counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestRead {
    pub checks: CheckSet,
    pub rate: Option<RateLimit>,
}

/// The checks of the base branch's head and of `base_at_push`, the base
/// head of Owlshift's last push, each complete; `at_push` is `None` when it
/// was not asked or is no commit GitHub knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseChecks {
    pub head: CommitId,
    pub at_head: Vec<Check>,
    pub at_push: Option<Vec<Check>>,
    pub rate: Option<RateLimit>,
}

/// A review submitted on a pull request (OWL-307, architecture section 5,
/// "Review comments on the pull request"): never a pending one, which only
/// its author sees and has not sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    /// GitHub's id of the review, the one its REST API gives; past 2^32.
    pub id: u64,
    /// Its author's login; `None` for an account GitHub no longer knows.
    pub author: Option<String>,
    /// `COMMENTED`, `APPROVED`, `CHANGES_REQUESTED` or `DISMISSED`, as
    /// GitHub writes it: the state plays no part in what Owlshift does.
    pub state: String,
    pub submitted_at: Timestamp,
    /// Empty when the review has inline comments only.
    pub body: String,
    /// The commit the review was made on, when GitHub still has it.
    pub commit: Option<CommitId>,
    /// Its inline comments, in GitHub's order: the first 100.
    pub comments: Vec<InlineComment>,
}

/// An inline comment of a review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineComment {
    /// The file, from the repository's root.
    pub path: String,
    /// Its line in the pull request's head; `None` once a later push made
    /// the comment outdated.
    pub line: Option<u32>,
    pub body: String,
}

/// The reviews of a pull request, oldest first: the newest 100 submitted
/// ones, and the read's cost when GitHub reported it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewsRead {
    pub reviews: Vec<Review>,
    pub rate: Option<RateLimit>,
}

/// What a check set says about the pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Open, no conflict, at least one check, and every check passed.
    Green,
    /// A check is not finished, or mergeability is not computed yet.
    Pending,
    /// A check failed, or the pull request conflicts with its base.
    Red,
    /// Open and mergeable, but the commit has no check at all: never green,
    /// since checks may simply not have started yet.
    NoChecks,
    /// The pull request is closed or merged.
    NotOpen,
}

impl CheckSet {
    /// The verdict, in this order: not open; red (a failed check or a
    /// conflict); pending (an unfinished check or unknown mergeability); no
    /// checks; green.
    pub fn verdict(&self) -> Verdict {
        let any = |state| self.checks.iter().any(|check| check.state == state);
        if self.state != PrState::Open {
            Verdict::NotOpen
        } else if any(CheckState::Failed) || self.mergeable == Mergeable::Conflicting {
            Verdict::Red
        } else if any(CheckState::Pending) || self.mergeable == Mergeable::Unknown {
            Verdict::Pending
        } else if self.checks.is_empty() {
            Verdict::NoChecks
        } else {
            Verdict::Green
        }
    }
}

/// What a token reads of a repository's default branch (OWL-334): a pull
/// request opens only on refs the token can read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultBranch {
    /// The token reads it.
    Readable(Branch),
    /// The repository has no default branch yet: it holds no commit.
    Empty,
    /// The forge refused the read, in its own words: a token that sees the
    /// repository but not its contents.
    Forbidden(String),
}

/// What went wrong talking to a forge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The repository or pull request does not exist, or the credentials
    /// cannot see it.
    NotFound,
    /// The forge refused the credentials.
    Unauthorized,
    /// The pull request's head is not the expected commit, or moved while
    /// its checks were read: read again once the push has landed.
    /// [`Error::head`] is the head read, when one was.
    HeadMoved,
    /// The forge's budget is spent: a GraphQL `RATE_LIMITED` error, or HTTP
    /// 403 or 429 with no budget left or a `Retry-After`. Nothing is read
    /// again before [`Error::resets_at`], when the answer gave it.
    RateLimited,
    /// The GitHub App's token can no longer be refreshed: GitHub refused
    /// its refresh (a revoked authorization, a refresh token past its life
    /// or already used), or a rotation was lost before its keychain write
    /// (architecture section 8, the refresh-failure rule). Trying again
    /// cannot help, and no other credential is tried: the message names
    /// `owlshift init`, which signs in again.
    SignInAgain,
    /// Anything else: transport, a refused request, a malformed or
    /// incomplete answer.
    Other,
}

/// A forge call that failed. The message never carries a credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    /// For [`ErrorKind::HeadMoved`], the head read.
    pub head: Option<CommitId>,
    /// For [`ErrorKind::RateLimited`], when the budget resets.
    pub resets_at: Option<Timestamp>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            head: None,
            resets_at: None,
        }
    }

    /// The error with the head read.
    pub fn with_head(mut self, head: CommitId) -> Self {
        self.head = Some(head);
        self
    }

    /// The error with the time the budget resets.
    pub fn with_reset(mut self, at: Option<Timestamp>) -> Self {
        self.resets_at = at;
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Other, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(state: CheckState) -> Check {
        Check {
            kind: CheckKind::Status,
            name: "ci".to_owned(),
            state,
            raw: String::new(),
            required: false,
            url: None,
            id: None,
            description: None,
        }
    }

    fn set(state: PrState, mergeable: Mergeable, checks: &[CheckState]) -> CheckSet {
        CheckSet {
            pull_request: 1,
            head: CommitId::new("a".repeat(40)).unwrap(),
            state,
            mergeable,
            merge_state: "CLEAN".to_owned(),
            checks: checks.iter().copied().map(check).collect(),
        }
    }

    #[test]
    fn the_verdict_needs_every_check_passed_on_an_open_mergeable_pull_request() {
        use CheckState::*;
        use Mergeable::{Conflicting, Unknown};
        let open = |m, c: &[CheckState]| set(PrState::Open, m, c).verdict();
        assert_eq!(
            open(Mergeable::Mergeable, &[Passed, Passed]),
            Verdict::Green
        );
        assert_eq!(
            open(Mergeable::Mergeable, &[Passed, Pending]),
            Verdict::Pending
        );
        assert_eq!(open(Unknown, &[Passed]), Verdict::Pending);
        // A failure wins over a check still running.
        assert_eq!(open(Mergeable::Mergeable, &[Pending, Failed]), Verdict::Red);
        assert_eq!(open(Conflicting, &[Passed]), Verdict::Red);
        assert_eq!(open(Mergeable::Mergeable, &[]), Verdict::NoChecks);
        assert_eq!(open(Unknown, &[]), Verdict::Pending);
        for state in [PrState::Closed, PrState::Merged] {
            assert_eq!(
                set(state, Mergeable::Mergeable, &[Passed]).verdict(),
                Verdict::NotOpen
            );
        }
    }

    #[test]
    fn repositories_parse_from_names_and_github_remotes() {
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        assert_eq!((repo.owner(), repo.name()), ("pitchopp", "owlshift"));
        assert_eq!(repo.to_string(), "pitchopp/owlshift");
        for url in [
            "git@github.com:pitchopp/owlshift.git",
            "git@github.com:pitchopp/owlshift",
            "ssh://git@github.com/pitchopp/owlshift.git",
            "https://github.com/pitchopp/owlshift.git",
            "https://github.com/pitchopp/owlshift/",
            "https://token@github.com/pitchopp/owlshift.git",
        ] {
            assert_eq!(Repo::from_github_remote(url).unwrap(), repo, "{url}");
        }
        for bad in ["owlshift", "a/b/c", "../x", "o/", "o/n n"] {
            assert!(Repo::parse(bad).is_err(), "{bad}");
        }
        for bad in [
            "https://gitlab.com/o/n.git",
            "git@example.com:o/n.git",
            "https://github.com.example.invalid/o/n.git",
            // An `@` in the path is not user info: the host is example.invalid.
            "https://example.invalid/@github.com/o/n.git",
        ] {
            assert!(Repo::from_github_remote(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn branch_names_follow_gits_rules() {
        for good in ["main", "sghirma/owl-17-add", "a.b/c_d", "x@y"] {
            assert!(Branch::new(good).is_ok(), "{good}");
        }
        for bad in [
            "", "-f", "a..b", "a b", "a:b", "a~1", "a^", "a?", "a*", "a[", "a\\b", "a/", "/a",
            "a//b", ".a", "a/.b", "a.lock", "a.", "@", "a@{b", "a\tb",
        ] {
            assert!(Branch::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn commit_ids_are_full_lowercase_hex() {
        assert!(CommitId::new("0".repeat(40)).is_ok());
        assert!(CommitId::new("f".repeat(64)).is_ok());
        for bad in ["abc", &"A".repeat(40), &"g".repeat(40), &"0".repeat(41)] {
            assert!(CommitId::new(bad).is_err(), "{bad}");
        }
    }
}
