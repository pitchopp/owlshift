//! Why a command refused to start, and whether trying again later may serve
//! (OWL-435, architecture section 5, "The background service", "Restarts").
//!
//! A served watch that refuses before its loop ends with an exit code of its
//! own when no restart can mend the refusal, and the daemon pauses its
//! project rather than start it again on backoff: a keychain that refuses
//! its secrets, a credential the tracker or forge rejects, a project with no
//! project file accepted on its default branch, a checkout registered from a
//! folder that is gone. A refusal is lasting unless it says otherwise: only
//! a tracker or forge that did not answer, and a project lock another
//! command held, are passing, a restart trying again.

use std::fmt;

/// A refusal, with whether a restart may mend it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// Why, in words for a person.
    pub message: String,
    /// Whether no restart can mend it: a person must act first.
    pub lasting: bool,
}

impl Refusal {
    /// A refusal a restart may mend: a service that did not answer, a lock
    /// another command held.
    pub fn passing(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            lasting: false,
        }
    }
}

impl From<String> for Refusal {
    /// A lasting refusal.
    fn from(message: String) -> Self {
        Self {
            message,
            lasting: true,
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
