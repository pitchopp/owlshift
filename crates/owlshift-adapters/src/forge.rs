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
pub mod push;

use std::fmt;

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
        let invalid = || invalid(format!("{text:?} is not a repository of the form owner/name"));
        let (owner, name) = text.split_once('/').ok_or_else(invalid)?;
        let valid = |part: &str| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        if !valid(owner) || !valid(name) {
            return Err(invalid());
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    /// The repository a GitHub remote URL points to: `git@github.com:o/r.git`,
    /// `ssh://git@github.com/o/r.git` or `https://github.com/o/r.git`, with or
    /// without the `.git` suffix. Any other host is refused.
    pub fn from_github_remote(url: &str) -> Result<Self, Error> {
        let path = url
            .strip_prefix("git@github.com:")
            .or_else(|| url.strip_prefix("ssh://git@github.com/"))
            .or_else(|| {
                let rest = url.strip_prefix("https://")?;
                let rest = rest.split_once('@').map_or(rest, |(_, host)| host);
                rest.strip_prefix("github.com/")
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
        let bad_component = name.split('/').any(|part| {
            part.is_empty() || part.starts_with('.') || part.ends_with(".lock")
        });
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
    /// The commit the checks belong to: the one the caller expected.
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
    HeadMoved,
    /// Anything else: transport, a refused request, a malformed or
    /// incomplete answer.
    Other,
}

/// A forge call that failed. The message never carries a credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
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
        assert_eq!(open(Mergeable::Mergeable, &[Passed, Passed]), Verdict::Green);
        assert_eq!(open(Mergeable::Mergeable, &[Passed, Pending]), Verdict::Pending);
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
        for bad in [
            "owlshift",
            "a/b/c",
            "../x",
            "o/",
            "o/n n",
            "https://gitlab.com/o/n.git",
            "git@example.com:o/n.git",
        ] {
            let error = Repo::parse(bad)
                .and_then(|_| Repo::from_github_remote(bad))
                .unwrap_err();
            assert!(!error.message.is_empty(), "{bad}");
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
