//! Trackers: where tickets, their comments and their visible stage live.
//!
//! The [`Tracker`] trait is what the runner needs from a tracker, with the
//! [`Capability`] rows of architecture section 6 that each adapter declares.
//! It was extracted at the second implementation (Linear, after the Markdown
//! tracker) and is not frozen: principle 8 freezes a contract only once three
//! implementations exist. It grows with the steps that need more: the
//! visible stage came with P2 (OWL-137), listing admitted tickets and reading
//! the admission gesture with P4 (OWL-246, architecture section 5, "The
//! drain"), filing proposals and finding them again with P4 too (OWL-252).

pub mod linear;
pub mod markdown;

use std::fmt;

use jiff::Timestamp;

use owlshift_contracts::Priority;
use owlshift_contracts::config::Admit;
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

    /// The name of the state the ticket shows now, as a person sees it on
    /// the tracker: watch reads it before a repair run, which a ticket a
    /// person moved to a state Owlshift does not work in never gets
    /// (OWL-112, architecture section 5, "A ticket a person moved"). An
    /// error by default: a tracker that cannot tell gets no repair run.
    fn stage(&self, _id: &TicketId) -> Result<String, Error> {
        Err(Error::new(
            ErrorKind::Other,
            "this tracker does not read a ticket's stage",
        ))
    }

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

    /// The tickets of the ready column that carry the admission gesture,
    /// each with the tickets that block it, every page read (OWL-246). The
    /// tracker's own order: the runner sorts. An error by default, and for
    /// the `delegation` gesture, which is not built.
    fn admitted(&self, _column: &Column<'_>) -> Result<Vec<Listed>, Error> {
        Err(Error::new(
            ErrorKind::Other,
            "this tracker does not list admitted tickets",
        ))
    }

    /// Who made the admission gesture on a ticket: the actor of the latest
    /// history entry that added the label (or moved the ticket to the
    /// state), or the ticket's creator when none did (architecture section
    /// 5, "By whom"). Read once, before the ticket's first intake, so it is
    /// not part of the listing. An error by default, and for the
    /// `delegation` gesture.
    fn gesture(&self, _id: &TicketId, _admit: &Admit) -> Result<Gesture, Error> {
        Err(Error::new(
            ErrorKind::Other,
            "this tracker does not read an admission gesture",
        ))
    }

    /// The type of the state a ticket shows, for a blocker the text
    /// convention names (OWL-250, architecture section 5, "Blockers"): the
    /// drain reads each "Blocked by" identifier that is not among the
    /// ticket's relations once a pass. `Ok(None)` when the tracker gives the
    /// state no type (a Markdown stage other than `Done`). A ticket the
    /// tracker does not know is an [`ErrorKind::NotFound`] error, which alone
    /// makes the blocker unknown; any other error counts it open. An error
    /// by default.
    fn state_type(&self, _id: &TicketId) -> Result<Option<StateType>, Error> {
        Err(Error::new(
            ErrorKind::Other,
            "this tracker does not read a ticket's state type",
        ))
    }

    /// Files a follow-up in the tracker's proposals inbox (OWL-252,
    /// architecture section 5, "Follow-ups"): the team's triage state, or
    /// without one its first backlog state with the [`PROPOSED_LABEL`]; in
    /// the parent's project, related to the parent, and blocked by it when
    /// the follow-up needs its code. Never in the ready column: a target
    /// state named [`Proposal::ready`] is refused before anything is written.
    ///
    /// An error means nothing was filed, and the proposal stays owed. Once
    /// the ticket exists, what goes wrong after it (a relation not written,
    /// the ticket answered in another state than the one asked) is kept in
    /// [`Filed::problems`] and the call still succeeds: the next pass would
    /// find the ticket by its fingerprint and count it as filed, so an error
    /// then would lose the problem without a word. An error by default.
    fn propose(&self, _proposal: &Proposal<'_>) -> Result<Filed, Error> {
        Err(Error::new(
            ErrorKind::Other,
            "this tracker does not file proposals",
        ))
    }

    /// The two exact searches a proposal is found again by (architecture
    /// section 5, "Not twice"): the tickets of `team` not completed that
    /// have the title `title` in any case, or whose description contains
    /// `marker`, the fingerprint's part of the proposal's footer as the
    /// runner renders it. Archived tickets count: a declined proposal stays
    /// declined once the team archives it. Each ticket once, with its
    /// description, from which the runner reads whose proposal it is.
    ///
    /// An error means the tracker could not tell, never that it found
    /// nothing: the runner then files nothing. An error by default.
    fn proposed(
        &self,
        _team: Option<&str>,
        _title: &str,
        _marker: &str,
    ) -> Result<Vec<Found>, Error> {
        Err(Error::new(
            ErrorKind::Other,
            "this tracker does not search proposals",
        ))
    }
}

/// The label of a proposal on a team without a triage state (architecture
/// section 6, "A proposals inbox for follow-ups").
pub const PROPOSED_LABEL: &str = "agent:proposed";

/// A follow-up to file, as the runner rendered it.
#[derive(Clone, Copy, Debug)]
pub struct Proposal<'a> {
    /// The ticket whose run proposed it.
    pub parent: &'a TicketId,
    pub title: &'a str,
    /// Markdown, the `[owlshift] PROPOSAL` footer included.
    pub description: &'a str,
    /// Whether the follow-up needs the parent's code: the parent then
    /// blocks it.
    pub blocked_by_parent: bool,
    /// The ready state's name, `[tracker].states.ready`. A proposal filed
    /// there would enter the queue without a person (principle 6), so the
    /// adapter refuses a target state of that exact name before any write:
    /// the floor's check, made where the target is known.
    pub ready: &'a str,
}

/// A proposal the tracker filed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filed {
    pub id: TicketId,
    /// The name of the state the tracker answered the ticket in.
    pub state: String,
    /// What went wrong once the ticket existed; empty when nothing did.
    pub problems: Vec<Error>,
}

/// A ticket one of the searches of [`Tracker::proposed`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub id: TicketId,
    /// Markdown; empty when the ticket has none.
    pub description: String,
}

/// What [`Tracker::admitted`] reads: the ready column of a project.
#[derive(Clone, Copy, Debug)]
pub struct Column<'a> {
    /// The project's `[tracker].team`; required by Linear, ignored by the
    /// Markdown tracker.
    pub team: Option<&'a str>,
    /// The ready state's name, `[tracker].states.ready`, matched exactly.
    pub ready: &'a str,
    /// The admission gesture, `[tracker].admit`: a label gesture lists the
    /// tickets carrying that label (matched exactly), a state gesture every
    /// ticket of the column.
    pub admit: &'a Admit,
}

/// A ticket of the ready column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub ticket: Ticket,
    /// When the tracker created it; `None` on a tracker that keeps no time
    /// (the Markdown tracker), whose ties then go to the identifier.
    pub created_at: Option<Timestamp>,
    /// The tickets that block it by the tracker's relations, once each. The
    /// text convention is read by the core, not here.
    pub blockers: Vec<Blocker>,
}

/// A ticket that blocks another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocker {
    pub id: TicketId,
    /// The type of the state it shows; `None` when the tracker gives it no
    /// type (a Markdown stage other than `Done`) or does not know the
    /// ticket. Only [`StateType::Completed`] releases a ticket: a canceled
    /// or duplicate blocker, or one without a type, keeps it waiting
    /// (architecture section 5, "Blockers").
    pub state: Option<StateType>,
}

/// The type of a tracker state: Linear's `WorkflowState.type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StateType {
    Triage,
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
    Duplicate,
}

/// Who made a ticket's admission gesture, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gesture {
    /// An [`Author::Account`] only when a person's account made it: an app
    /// user (Owlshift's own included), a bot, an integration or an account
    /// the tracker no longer knows is [`Author::Other`], which never admits.
    pub account: Author,
    /// The tracker's time of the gesture; `None` on a tracker that keeps
    /// no history (the Markdown tracker).
    pub at: Option<Timestamp>,
}

/// The `delegation` gesture is not built: decision D8's app takes no
/// delegation scope (architecture section 5, "The gesture").
pub(crate) fn no_delegation() -> Error {
    Error::new(
        ErrorKind::Other,
        "the `delegation` admission gesture is not built: admit with a label or a state",
    )
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
    /// The comment it replies to, in a tracker that threads replies: how the
    /// decider reverses a DECISION (OWL-146); `None` for a top-level one.
    pub parent: Option<String>,
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
