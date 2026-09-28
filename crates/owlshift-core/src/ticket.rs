//! Tickets: units of work read from the tracker, admitted by a human gesture.

use crate::pipeline::Pipeline;
use crate::resource::Resource;
use crate::state::TicketState;
use crate::vocab::{Priority, Variant};

/// What the core knows of a ticket. `K` is the ticket's identifier; the core
/// needs only its identity, and its format is the tracker's and the
/// contracts' concern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket<K> {
    pub id: K,
    pub priority: Priority,
    pub admission: Admission,
    /// The tickets that block this one.
    pub blocked_by: Vec<Blocker<K>>,
    /// The pipeline variant: picked at intake, or forced by a label.
    pub variant: Variant,
    /// The resources declared at intake.
    pub resources: Vec<Resource>,
    pub state: TicketState,
}

impl<K> Ticket<K> {
    /// The pipeline of the ticket's variant.
    pub const fn pipeline(&self) -> Pipeline {
        Pipeline::new(self.variant)
    }
}

/// Whether a person put the ticket in the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Admission {
    /// No admission gesture from a person the project lists.
    NotAdmitted,
    /// Admitted by delegation, a label or a state, from a listed person.
    Admitted,
    /// An exclusion label keeps it out for good, whatever else it carries.
    Excluded,
}

/// A ticket that blocks another.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Blocker<K> {
    pub id: K,
    /// Whether the tracker reports it done; only a done blocker releases the
    /// ticket it blocks.
    pub done: bool,
}
