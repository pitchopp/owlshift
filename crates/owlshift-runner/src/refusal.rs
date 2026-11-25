//! Why a command refused to start, and what may mend it (OWL-435, OWL-479,
//! architecture section 5, "The background service", "Restarts").
//!
//! A served watch that refuses before its loop ends with an exit code of its
//! own when no restart can mend the refusal, and the daemon pauses its
//! project rather than start it again on backoff: a keychain that refuses
//! its secrets, a credential the tracker or forge rejects, a project with no
//! project file accepted on its default branch, a checkout registered from a
//! folder that is gone. A refusal is lasting unless it says otherwise. A
//! tracker or forge that answered with an error a retry may clear, and a
//! project lock another command held, are mended by a restart; a tracker or
//! forge that gave no answer at all, by waiting for the network: a watch
//! the daemon starts waits in its own process rather than end.
//!
//! A refusal of the GitHub App's token that only a new sign-in mends
//! (OWL-650) is said in a person's words once its command knows the folder
//! to sign in from ([`Refusal::in_checkout`]): what happened, what it stops,
//! the command and its folder, and what the command keeps, the same text the
//! daemon's pause, `owlshift status`, the UI, the digest and `owlshift
//! doctor` show. The technical words, which name the keychain entry and
//! GitHub's error, stay as its [`detail`](Refusal::detail).

use std::fmt;
use std::path::Path;

use owlshift_adapters::forge::Repo;

/// What may mend a refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mend {
    /// Nothing: a person must act first.
    Lasting,
    /// A restart: a service that answered with an error a retry may clear,
    /// a lock another command held.
    Restart,
    /// The network: a tracker or forge that gave no answer at all.
    Network,
}

/// What a person does to mend a lasting refusal, when its command words it
/// for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Need {
    /// Sign in to GitHub again: GitHub refused the GitHub App's token, or
    /// its keychain entry no longer reads (OWL-650).
    GitHubSignIn,
}

/// A refusal, with what may mend it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Why, in words for a person.
    pub message: String,
    /// The technical words behind [`message`](Self::message), when it was
    /// reworded for a person: the command prints them on a line of their
    /// own just before it, which daemon.log keeps.
    pub detail: Option<String>,
    mend: Mend,
    need: Option<Need>,
}

impl Refusal {
    /// A refusal a restart may mend: a service that answered with an error a
    /// retry may clear, a lock another command held.
    pub fn passing(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: None,
            mend: Mend::Restart,
            need: None,
        }
    }

    /// A tracker or forge that gave no answer at all (OWL-479): mended once
    /// the network is up, and by a restart as a passing refusal is.
    pub fn unreachable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            detail: None,
            mend: Mend::Network,
            need: None,
        }
    }

    /// A lasting refusal only a new sign-in to GitHub mends (OWL-650): the
    /// GitHub App's token was refused, its rotation lost, or its keychain
    /// entry does not read or is gone. `message` holds the technical words,
    /// until [`in_checkout`](Self::in_checkout) words it for a person.
    pub fn sign_in_to_github(message: impl Into<String>) -> Self {
        Self {
            need: Some(Need::GitHubSignIn),
            ..Self::from(message.into())
        }
    }

    /// This refusal said for the project of `repo` whose command signs in
    /// from `checkout`: a refusal only a new sign-in mends takes
    /// [`sign_in_notice`](crate::forge::sign_in_notice) as its message, with
    /// the last sentence when `resumes`, as for the daemon's watch, and
    /// keeps its technical words as its [`detail`](Self::detail). Any other
    /// refusal is returned as it is.
    #[must_use]
    pub fn in_checkout(self, repo: &Repo, checkout: &Path, resumes: bool) -> Self {
        match self.need {
            Some(Need::GitHubSignIn) if self.detail.is_none() => Self {
                message: crate::forge::sign_in_notice(repo, checkout, resumes),
                detail: Some(self.message),
                ..self
            },
            _ => self,
        }
    }

    /// Whether no restart can mend it: a person must act first.
    pub fn lasting(&self) -> bool {
        self.mend == Mend::Lasting
    }

    /// Whether it waits for the network: no answer at all.
    pub fn unanswered(&self) -> bool {
        self.mend == Mend::Network
    }
}

impl From<String> for Refusal {
    /// A lasting refusal.
    fn from(message: String) -> Self {
        Self {
            message,
            detail: None,
            mend: Mend::Lasting,
            need: None,
        }
    }
}

impl From<&str> for Refusal {
    /// A lasting refusal.
    fn from(message: &str) -> Self {
        message.to_owned().into()
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
