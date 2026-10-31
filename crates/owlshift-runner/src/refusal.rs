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

use std::fmt;

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

/// A refusal, with what may mend it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Why, in words for a person.
    pub message: String,
    mend: Mend,
}

impl Refusal {
    /// A refusal a restart may mend: a service that answered with an error a
    /// retry may clear, a lock another command held.
    pub fn passing(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            mend: Mend::Restart,
        }
    }

    /// A tracker or forge that gave no answer at all (OWL-479): mended once
    /// the network is up, and by a restart as a passing refusal is.
    pub fn unreachable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            mend: Mend::Network,
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
            mend: Mend::Lasting,
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
