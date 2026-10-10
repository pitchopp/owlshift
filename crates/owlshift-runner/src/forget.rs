//! `owlshift forget TICKET` (OWL-202): starts over what Owlshift keeps on a
//! ticket between commands ([`ticket_ref::forget`]), for a ticket ref that
//! has no room for its next write or cannot be read, so that neither needs
//! git run by hand in the dedicated checkout.
//!
//! It takes the project's lock, as `do`, `continue` and `watch` do, so it
//! never changes a ref one of them read; it reads neither the tracker nor
//! the keychain, and records one `decision` event.

use std::fmt;
use std::fs;

use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{TicketQuestions, Waiting, ticket_ref};
use serde_json::json;

use crate::events::{EventSink, data};
use crate::executor::Git;
use crate::on_demand::Stop;
use crate::project::ProjectDirs;
use crate::ticket_ref::{self, Forgot, Previous, TicketRecord};

/// What `owlshift forget` did to a ticket's ref.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forgotten {
    pub ticket: TicketId,
    pub forgot: Forgot,
}

/// Forgets what the ticket ref of `ticket` keeps, under the project's lock.
/// Refused with nothing to forget when the project has no dedicated
/// checkout or the ticket no ref, and while another command holds the
/// project.
pub fn forget(
    git: &Git,
    dirs: &ProjectDirs,
    ticket: &TicketId,
    sink: &mut EventSink<'_>,
) -> Result<Forgotten, Stop> {
    let checkout = dirs.checkout();
    // Before the lock, which makes the project's directory: a project with
    // no dedicated checkout keeps nothing, and gets no directory. A checkout
    // removed once the lock is held fails the git commands below.
    if fs::symlink_metadata(&checkout).is_err() {
        return Err(nothing_to_forget(ticket));
    }
    // `Stop::Busy` names `do`, `continue` and `watch`: forget holds the lock
    // for the moment of one ref write, so a command typed meanwhile reads
    // that message only for that moment.
    let _lock = match dirs.lock() {
        Ok(Some(lock)) => lock,
        Ok(None) => return Err(Stop::Busy(dirs.root().to_owned())),
        Err(error) => return Err(Stop::Refused(format!("the project's lock: {error}"))),
    };
    let forgot = ticket_ref::forget(git, &checkout, ticket)
        .map_err(|e| Stop::Refused(format!("forgetting {ticket}: {e}")))?
        .ok_or_else(|| nothing_to_forget(ticket))?;
    let event = match &forgot {
        Forgot::Rewritten {
            previous,
            commit,
            from,
            kept,
        } => {
            let mut event = data([
                ("forgotten", json!("rewritten")),
                ("previous", json!(previous)),
                ("commit", json!(commit)),
                ("round", json!(kept.state.round)),
            ]);
            match from {
                Previous::Read(record) => {
                    event.insert("asks".to_owned(), json!(record.questions.asks.len()));
                    event.insert(
                        "decisions".to_owned(),
                        json!(record.questions.decisions.len()),
                    );
                }
                Previous::Unreadable(why) => {
                    event.insert("unreadable".to_owned(), json!(why));
                }
            }
            event
        }
        Forgot::Unchanged { commit, kept } => data([
            ("forgotten", json!("unchanged")),
            ("commit", json!(commit)),
            ("round", json!(kept.state.round)),
        ]),
        Forgot::Deleted { previous, why } => data([
            ("forgotten", json!("deleted")),
            ("previous", json!(previous)),
            ("unreadable", json!(why)),
        ]),
    };
    sink.emit(ticket, None, EventKind::Decision, event);
    Ok(Forgotten {
        ticket: ticket.clone(),
        forgot,
    })
}

fn nothing_to_forget(ticket: &TicketId) -> Stop {
    Stop::Refused(format!(
        "nothing to forget: Owlshift keeps nothing on {ticket}"
    ))
}

impl fmt::Display for Forgotten {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ticket = &self.ticket;
        let name = ticket_ref(ticket);
        let after = |f: &mut fmt::Formatter<'_>, previous: &str, commit: &str| {
            write!(
                f,
                " {name} now points to {commit}, whose parent {previous} holds the previous \
                 record. The comments stay on the ticket; its stage on the tracker moves at the \
                 next `owlshift do {ticket}`, which starts it from Ready."
            )
        };
        match &self.forgot {
            Forgot::Rewritten {
                previous,
                commit,
                from: Previous::Read(record),
                kept,
            } => {
                write!(
                    f,
                    "Forgot what Owlshift kept on {ticket}: dropped {}; kept {}.",
                    list(&dropped(record)),
                    list(&kept_parts(kept))
                )?;
                after(f, previous, commit)
            }
            Forgot::Rewritten {
                previous,
                commit,
                from: Previous::Unreadable(why),
                kept,
            } => {
                write!(
                    f,
                    "Forgot what Owlshift kept on {ticket}, whose record could not be read: \
                     {why}. Kept {}, from what still reads; dropped the rest.",
                    list(&kept_parts(kept))
                )?;
                after(f, previous, commit)
            }
            Forgot::Unchanged { commit, kept } => write!(
                f,
                "Nothing to forget on {ticket}: {name}, at {commit}, keeps only {}.",
                list(&kept_parts(kept))
            ),
            Forgot::Deleted { previous, why } => write!(
                f,
                "Deleted {name}: {why}. It pointed to {previous}, and nothing of it is kept: the \
                 comments stay on the ticket, and the next `owlshift do {ticket}` asks from round \
                 1."
            ),
        }
    }
}

/// What forgetting `record` drops, every part it held.
fn dropped(record: &TicketRecord) -> Vec<String> {
    let questions: &TicketQuestions = &record.questions;
    let mut parts = Vec::new();
    if let (Some(first), Some(last)) = (questions.asks.first(), questions.asks.last()) {
        let rounds = if first.round == last.round {
            format!("round {}", first.round)
        } else {
            format!("rounds {} to {}", first.round, last.round)
        };
        parts.push(format!(
            "{} of {rounds} with their verdicts",
            counted(questions.asks.len(), "ask", "asks")
        ));
    }
    if !questions.decisions.is_empty() {
        parts.push(format!(
            "{} of the resolver",
            counted(questions.decisions.len(), "decision", "decisions")
        ));
    }
    if questions.checked_through.is_some() {
        parts.push("what the last answer check read".to_owned());
    }
    let round = record.state.round;
    parts.push(match record.state.waiting {
        None => "its state".to_owned(),
        Some(Waiting::Parked) => "its parked state".to_owned(),
        Some(Waiting::NeedsInput) => format!(
            "its state, waiting for the answers of round {round}: those questions are \
             abandoned, and the next Build run asks what it still needs"
        ),
        Some(Waiting::ParkedAwaitingInput) => format!(
            "its parked state, which waited for the answers of round {round}: those questions \
             are abandoned, and the next Build run asks what it still needs"
        ),
    });
    parts
}

/// What a forgotten record keeps.
fn kept_parts(kept: &TicketRecord) -> Vec<String> {
    let round = kept.state.round;
    let mut parts = vec![format!(
        "round {round} (the next round is {})",
        round.saturating_add(1)
    )];
    if kept.questions.build_refusal.is_some() {
        parts.push("Build's held refusal".to_owned());
    }
    if kept.questions.resolver_refusal.is_some() {
        parts.push("the resolver's refusal".to_owned());
    }
    parts
}

/// `n` and its noun.
fn counted(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The parts joined: `a`, `a and b`, `a, b and c`.
fn list(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}
