//! Notifiers: how Owlshift tells one person that they have become the
//! blocker (architecture section 6, "Send one short line to one person";
//! scenario S6).
//!
//! The first notifier, the desktop one of P2, lives in the runner, which
//! runs programs; it declares no capability yet. The trait comes before a
//! second notifier, unlike the other kinds, so that tests can record what
//! would be shown instead of showing it. The tracker mention and the webhook
//! of the table come with the steps that build them.

use std::fmt;

/// Sends one short line to the one person a notifier reaches.
pub trait Notifier {
    /// Shows `line`. A notifier with nowhere to show it, such as a machine
    /// without a desktop session, shows nothing and succeeds.
    fn notify(&self, line: &str) -> Result<(), Error>;
}

/// Why a line could not be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
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
