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
//!
//! OWL-658 words the other lasting refusals of a credential or a secret the
//! same way: no GitHub token stored, a keychain that does not read, no
//! Linear API key, and a Linear app stored by half, refused or blind to the
//! team. Each says what stops, the fix in plain words with the folder to run
//! it in, and what the fix keeps; none tells to edit the keychain by service
//! and account but the choice of writing through the Linear API key.

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
#[derive(Clone, Debug, PartialEq, Eq)]
enum Need {
    /// Sign in to GitHub again: GitHub refused the GitHub App's token, or
    /// its keychain entry no longer reads (OWL-650).
    GitHubSignIn,
    /// Sign in to GitHub: no GitHub token is stored (OWL-658).
    GitHubToken,
    /// Unlock the system keychain: it does not open, or an entry does not
    /// read (OWL-658).
    Keychain,
    /// Give a Linear API key: none is stored (OWL-658).
    LinearKey,
    /// Store the Linear app again, or write through the key (OWL-658).
    LinearApp(crate::tracker::AppFault),
}

/// The sentence a notice ends with when the daemon tries its project again
/// by itself (OWL-650): a command typed in a terminal drops it.
pub(crate) const PICKED_UP_AGAIN: &str = "Owlshift picks the project up again within a minute.";

/// `notice`, with `last` after it when `resumes`.
pub(crate) fn resumed(mut notice: String, resumes: bool, last: &str) -> String {
    if resumes {
        notice.push(' ');
        notice.push_str(last);
    }
    notice
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
        Self::needing(message, Need::GitHubSignIn)
    }

    /// A lasting refusal for no GitHub token stored (OWL-658), worded with
    /// [`no_token_notice`](crate::forge::no_token_notice).
    pub fn no_github_token(message: impl Into<String>) -> Self {
        Self::needing(message, Need::GitHubToken)
    }

    /// A lasting refusal for a system keychain that does not open, or one of
    /// whose entries does not read (OWL-658), worded with
    /// [`keychain_notice`](crate::secrets::keychain_notice).
    pub fn keychain(message: impl Into<String>) -> Self {
        Self::needing(message, Need::Keychain)
    }

    /// A lasting refusal for no Linear API key stored (OWL-658), worded with
    /// [`linear_key_notice`](crate::tracker::linear_key_notice).
    pub fn no_linear_key(message: impl Into<String>) -> Self {
        Self::needing(message, Need::LinearKey)
    }

    /// A lasting refusal of the Linear app for `fault` (OWL-658), worded
    /// with [`AppFault::notice`](crate::tracker::AppFault::notice).
    pub fn linear_app(message: impl Into<String>, fault: crate::tracker::AppFault) -> Self {
        Self::needing(message, Need::LinearApp(fault))
    }

    /// A lasting refusal `need` mends, `message` its technical words.
    fn needing(message: impl Into<String>, need: Need) -> Self {
        Self {
            need: Some(need),
            ..Self::from(message.into())
        }
    }

    /// This refusal said for the project of `repo` whose command runs from
    /// `checkout`: a refusal a person mends takes its notice as its message,
    /// with its last sentence when `resumes`, as for the daemon's watch, and
    /// keeps its technical words as its [`detail`](Self::detail). A refusal
    /// already worded, and any other refusal, is returned as it is.
    #[must_use]
    pub fn in_checkout(self, repo: &Repo, checkout: &Path, resumes: bool) -> Self {
        if self.detail.is_some() {
            return self;
        }
        let notice = match &self.need {
            None => return self,
            Some(Need::GitHubSignIn) => crate::forge::sign_in_notice(repo, checkout, resumes),
            Some(Need::GitHubToken) => crate::forge::no_token_notice(repo, checkout, resumes),
            Some(Need::Keychain) => crate::secrets::keychain_notice(repo, resumes),
            Some(Need::LinearKey) => crate::tracker::linear_key_notice(repo, checkout, resumes),
            Some(Need::LinearApp(fault)) => fault.notice(repo, checkout, resumes),
        };
        Self {
            detail: Some(self.message),
            message: notice,
            ..self
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::TrackerAccounts;
    use crate::tracker::{AppFault, AppFaultKind, LINEAR_SECRETS};

    /// OWL-658: each refusal a person mends is said, in its checkout, with
    /// its own notice, lasting, its technical words kept as the detail, and
    /// once only; a refusal no person mends is returned as it is.
    #[test]
    fn each_need_is_said_with_its_notice_once() {
        let repo = Repo::parse("acme/web").unwrap();
        let folder = Path::new("/work/web");
        let fault = AppFault {
            kind: AppFaultKind::Half,
            accounts: TrackerAccounts::project(&repo, &LINEAR_SECRETS),
        };
        for (refusal, notice) in [
            (
                Refusal::sign_in_to_github("t"),
                crate::forge::sign_in_notice(&repo, folder, true),
            ),
            (
                Refusal::no_github_token("t"),
                crate::forge::no_token_notice(&repo, folder, true),
            ),
            (
                Refusal::keychain("t"),
                crate::secrets::keychain_notice(&repo, true),
            ),
            (
                Refusal::no_linear_key("t"),
                crate::tracker::linear_key_notice(&repo, folder, true),
            ),
            (
                Refusal::linear_app("t", fault.clone()),
                fault.notice(&repo, folder, true),
            ),
        ] {
            assert!(refusal.lasting(), "{refusal}");
            let said = refusal.in_checkout(&repo, folder, true);
            assert_eq!(said.message, notice);
            assert_eq!(said.detail.as_deref(), Some("t"));
            assert!(said.lasting(), "{said}");
            assert_eq!(said.clone().in_checkout(&repo, folder, false), said);
            assert!(said.message.ends_with("within a minute."), "{said}");
        }

        for other in [
            Refusal::from("no project file"),
            Refusal::passing("Linear answered 500"),
            Refusal::unreachable("no answer"),
        ] {
            assert_eq!(other.clone().in_checkout(&repo, folder, true), other);
        }
    }
}
