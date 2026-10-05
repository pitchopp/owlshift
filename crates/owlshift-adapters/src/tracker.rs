//! Trackers: where tickets, their comments and their visible stage live.
//!
//! The [`Tracker`] trait is what the runner needs from a tracker, with the
//! [`Capability`] rows of architecture section 6 that each adapter declares.
//! It was extracted at the second implementation (Linear, after the Markdown
//! tracker) and is not frozen: principle 8 freezes a contract only once three
//! implementations exist. It grows with the steps that need more: the
//! visible stage came with P2 (OWL-137), listing admitted tickets comes in
//! P4.

pub mod linear;
pub mod markdown;

use std::fmt;

use jiff::Timestamp;

use owlshift_contracts::Priority;
use owlshift_contracts::ids::TicketId;

/// What the runner needs from a tracker, as far as this build goes.
pub trait Tracker {
    /// The capabilities this adapter implements in this build.
    fn capabilities(&self) -> &'static [Capability];

    /// Reads a ticket.
    fn ticket(&self, id: &TicketId) -> Result<Ticket, Error>;

    /// Every comment on a ticket, oldest first.
    fn comments(&self, id: &TicketId) -> Result<Vec<Comment>, Error>;

    /// Posts a comment under the adapter's own identity and returns it as the
    /// tracker recorded it.
    fn post_comment(&self, id: &TicketId, body: &str) -> Result<Comment, Error>;

    /// Moves the ticket's visible stage to `state`, a state name of the
    /// tracker as the project's `[tracker].states` maps it. A tracker with a
    /// fixed set of states refuses a name outside it, never guessing a close
    /// one; the Markdown tracker takes any name.
    fn set_stage(&self, id: &TicketId, state: &str) -> Result<(), Error>;

    /// A link to the ticket that a person can open, for a notification
    /// (OWL-140). `None` when the tracker has none, and when it could not be
    /// read: a link is never worth failing for.
    fn ticket_url(&self, _id: &TicketId) -> Option<String> {
        None
    }

    /// The text that, written in a comment, makes the tracker notify the
    /// account `account` (OWL-157): on Linear, the person's profile link.
    /// `Ok(None)` when the tracker has no such text. An error when it has
    /// one but could not give this account's: the comment still goes
    /// without it, and the runner reports it (OWL-170). Unlike a missing
    /// [`ticket_url`](Self::ticket_url), which only loses a convenience, a
    /// missing mention loses the tracker's notification to the person, so
    /// the operator must be able to tell.
    fn mention(&self, _account: &str) -> Result<Option<String>, Error> {
        Ok(None)
    }
}

/// A row of the tracker capability table in architecture section 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Read a ticket: title, description, priority, assignee, labels, author.
    ReadTicket,
    /// List the admitted tickets.
    ListAdmitted,
    /// Read and write comments with author and last-edit time.
    Comments,
    /// Show the visible stage: ready, in progress, needs input, in review.
    VisibleStage,
    /// Blocked-by relations.
    BlockedBy,
    /// A proposals inbox for follow-ups.
    ProposalsInbox,
    /// A distinct agent identity.
    AgentIdentity,
    /// Webhooks.
    Webhooks,
}

impl Capability {
    /// Every row, in the table's order.
    pub const ALL: [Self; 8] = [
        Self::ReadTicket,
        Self::ListAdmitted,
        Self::Comments,
        Self::VisibleStage,
        Self::BlockedBy,
        Self::ProposalsInbox,
        Self::AgentIdentity,
        Self::Webhooks,
    ];

    /// Whether every tracker adapter must provide it.
    pub fn required(self) -> bool {
        matches!(
            self,
            Self::ReadTicket | Self::ListAdmitted | Self::Comments | Self::VisibleStage
        )
    }

    /// A short description, for reports such as `owlshift doctor`.
    pub fn describe(self) -> &'static str {
        match self {
            Self::ReadTicket => "read a ticket",
            Self::ListAdmitted => "list admitted tickets",
            Self::Comments => "read and post comments",
            Self::VisibleStage => "visible stage",
            Self::BlockedBy => "blocked-by relations",
            Self::ProposalsInbox => "proposals inbox",
            Self::AgentIdentity => "agent identity",
            Self::Webhooks => "webhooks",
        }
    }
}

/// A ticket as the runner sees it, whatever the tracker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub id: TicketId,
    pub title: String,
    /// Markdown; empty when the ticket has none.
    pub description: String,
    pub priority: Priority,
    pub assignee: Option<Person>,
    pub labels: Vec<String>,
    /// Who created the ticket. Later edits of the description by someone else
    /// do not change it: the tracker names the creator, not every editor. An
    /// adapter that cannot hold the creator's account to be the writer
    /// reports it as [`Author::Other`].
    pub author: Author,
}

/// A tracker account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Person {
    /// The tracker's stable identifier for the account.
    pub id: String,
    /// The name the tracker shows.
    pub name: String,
}

/// Who wrote a ticket or a comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Author {
    /// A tracker account; only an account can be a ticket's decider.
    Account(Person),
    /// Anything else the tracker names: a bot, an integration, an external
    /// user, an account the adapter does not hold to be the writer (a Linear
    /// issue's creator), or nobody it still knows.
    Other { name: String },
}

/// A comment on a ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    /// The tracker's identifier for the comment.
    pub id: String,
    pub author: Author,
    /// When the tracker recorded it. The times order a ticket's comments as
    /// the tracker lists them, those of one second included: a tracker that
    /// keeps whole seconds gives each comment of a second its own time, in
    /// its own order (OWL-136), since the runner compares times only.
    pub created_at: Timestamp,
    /// When its author last edited it; `None` if never edited.
    pub edited_at: Option<Timestamp>,
    pub body: String,
}

impl Author {
    /// The author the tracker could not name.
    pub fn unknown() -> Self {
        Self::Other {
            name: "unknown".to_owned(),
        }
    }
}

impl Comment {
    /// The time of its last edit: the creation time when never edited.
    pub fn last_edit(&self) -> Timestamp {
        self.edited_at.unwrap_or(self.created_at)
    }
}

/// What went wrong talking to a tracker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// The ticket does not exist, or the credentials cannot see it.
    NotFound,
    /// The tracker refused the credentials.
    Unauthorized,
    /// Anything else: transport, a refused request, a malformed answer.
    Other,
}

/// A tracker call that failed. The message never carries a credential.
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
