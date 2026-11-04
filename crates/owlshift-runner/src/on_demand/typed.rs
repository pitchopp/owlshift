//! A restart typed on the ticket (OWL-488; architecture section 5, "A
//! restart typed on the ticket"): the decider's reply `continue` on a parked
//! ticket, read by `owlshift watch`, restarts it as `owlshift continue`
//! does.
//!
//! - *Which comment* ([`restart_comment`]): the newest whose whole text,
//!   blanks around it and case aside, is `continue`, newer than the ticket's
//!   latest PARKED comment and than the last such comment the runner
//!   settled (`restart.json` in the ticket's folder, [`TicketRestart`]),
//!   and, once the decider is known, theirs. One comment restarts at most
//!   one park, and is answered at most once, whatever a watch restart
//!   forgets.
//! - *The restart* ([`OnDemand::restart_typed`]): `continue`'s, as one step
//!   of watch's: handed to watch as `continue --to-watch` hands it when
//!   watch takes the restarted ticket, run as plain `continue` otherwise (a
//!   park at Intake, a review loop's Verify whose Build is no longer the
//!   one to push).
//! - *A refusal* `continue` would print, a quarantine, an empty gate, a
//!   Verify that cannot resume, posts one REPLY;
//!   a read that fails is a `warning`, and the next pass tries again.

use owlshift_contracts::step::{Settled, TicketRestart};

use super::*;
use crate::step::write_atomically;

/// The file in a ticket's folder that keeps the last `continue` comment the
/// runner settled.
pub const RESTART_FILE: &str = "restart.json";

/// The comment a typed restart started from, which its `decision` event
/// names and `restart.json` keeps once the restart is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Typed {
    pub(crate) comment: String,
    pub(crate) at: Timestamp,
}

impl Typed {
    fn of(comment: &Comment) -> Self {
        Self {
            comment: comment.id.clone(),
            at: comment.created_at,
        }
    }
}

/// Whether a comment's text is the restart word alone: `continue`, blanks
/// around it and case aside.
pub fn is_restart_word(body: &str) -> bool {
    body.trim().eq_ignore_ascii_case("continue")
}

/// The `continue` comment of `thread` that restarts its parked ticket: the
/// newest one, by creation, newer than the latest PARKED comment and than
/// `settled`, the last one the runner settled; of the decider's own when
/// `decider` is given, of anyone's otherwise. None without a PARKED
/// comment: a park whose comment was never posted is restarted by the
/// command only.
pub fn restart_comment<'c>(
    thread: &'c [Comment],
    decider: Option<&Person>,
    settled: Option<&TicketRestart>,
) -> Option<&'c Comment> {
    let parked = thread
        .iter()
        .filter(|comment| {
            matches!(
                MarkedComment::parse(&comment.body),
                Ok(Some(marked)) if marked.header.kind == MarkerKind::Parked
            )
        })
        .map(|comment| comment.created_at)
        .max()?;
    let after = settled.map_or(parked, |settled| settled.at.max(parked));
    thread
        .iter()
        .filter(|comment| comment.created_at > after)
        .filter(|comment| settled.is_none_or(|settled| settled.comment != comment.id))
        .filter(|comment| is_restart_word(&comment.body))
        .filter(|comment| {
            decider.is_none_or(|decider| {
                comment_author(comment, decider).relation == Relation::Decider
            })
        })
        .max_by_key(|comment| comment.created_at)
}

/// Where `ticket`'s last settled `continue` comment is kept.
pub fn restart_file(dirs: &ProjectDirs, ticket: &TicketId) -> PathBuf {
    dirs.ticket_dir(ticket).join(RESTART_FILE)
}

/// The last `continue` comment the runner settled on `ticket`: none when
/// there is none, or when the file does not read, which at worst lets that
/// comment be settled again.
pub fn settled(dirs: &ProjectDirs, ticket: &TicketId) -> Option<TicketRestart> {
    let text = fs::read_to_string(restart_file(dirs, ticket)).ok()?;
    TicketRestart::parse(&text).ok()
}

/// Keeps `comment` as the last settled on `ticket`, how it was; a write
/// that fails is a `warning`.
pub(crate) fn settle(
    dirs: &ProjectDirs,
    ticket: &TicketId,
    comment: &str,
    at: Timestamp,
    how: Settled,
    sink: &mut EventSink<'_>,
) {
    let record = TicketRestart {
        format: Format,
        comment: comment.to_owned(),
        at,
        settled: how,
    };
    let written = fs::create_dir_all(dirs.ticket_dir(ticket))
        .and_then(|()| write_atomically(&restart_file(dirs, ticket), &record.render()));
    if let Err(error) = written {
        sink.emit(
            ticket,
            None,
            EventKind::Warning,
            data([
                ("what", json!("restart_not_kept")),
                ("comment", json!(comment)),
                ("reason", json!(error.to_string())),
            ]),
        );
    }
}

/// The `warning` of a typed restart that a failing read or write stopped
/// before the restart was kept: the next pass tries again.
fn unread(ticket: &TicketId, comment: &str, stop: &Stop, sink: &mut EventSink<'_>) {
    sink.emit(
        ticket,
        None,
        EventKind::Warning,
        data([
            ("what", json!("restart_unread")),
            ("comment", json!(comment)),
            ("reason", json!(stop.to_string())),
        ]),
    );
}

impl OnDemand<'_> {
    /// One step of watch's (OWL-488): the restart of parked `ticket` its
    /// decider typed on it, while its ticket ref, read under the ticket's
    /// lock, is still at `seen`. `comment` is the newest `continue` comment
    /// watch read. `None`, nothing restarted: the ref moved or no longer
    /// keeps a park, no comment counts (the newest settled as not counted),
    /// or a read failed (a `warning`, tried again at the next pass).
    pub fn restart_typed(
        &self,
        ticket: &TicketId,
        seen: &str,
        comment: &str,
        sink: &mut EventSink<'_>,
    ) -> Option<Result<Delivered, Stop>> {
        let mut p = match self.prepare(ticket, Command::Continue) {
            Ok(p) => p,
            Err(stop @ Stop::Refused(_)) => {
                unread(ticket, comment, &stop, sink);
                return None;
            }
            Err(stop) => return Some(Err(stop)),
        };
        let stored = p.stored.as_ref().filter(|stored| stored.commit == seen)?;
        let state = TicketState::try_from(&stored.record.state).ok()?;
        if !matches!(state.status(), Status::Parked { .. }) {
            return None;
        }
        let thread = match self.comments(ticket) {
            Ok(thread) => thread,
            Err(stop) => {
                unread(ticket, comment, &stop, sink);
                return None;
            }
        };
        let kept = settled(self.dirs, ticket);
        // A decider that could not be resolved, its zones' owners unread, is
        // a read that failed; a ticket that has none settles the comment.
        if let Err(why) = &p.decider
            && !why.starts_with(&format!("{ticket} has no decider"))
        {
            unread(ticket, comment, &Stop::Refused(why.clone()), sink);
            return None;
        }
        let decider = p.decider.as_ref().ok().map(|found| person(found, &p.found));
        let counting = decider
            .as_ref()
            .and_then(|decider| restart_comment(&thread, Some(decider), kept.as_ref()));
        let Some(counting) = counting else {
            // None of the comments is the decider's, or the ticket has no
            // decider: the newest is settled, so none is read again.
            if let Some(newest) = restart_comment(&thread, None, kept.as_ref()) {
                let at = newest.created_at;
                settle(self.dirs, ticket, &newest.id, at, Settled::NotCounted, sink);
            }
            return None;
        };
        let typed = Typed::of(counting);
        p.typed = Some(typed.clone());
        p.stepping = true;
        // `carry.json` dropped, as for any person's `continue`; a cancel
        // standing is not withdrawn: a step never removes what stops it.
        self.drop_carry(ticket, sink);
        let outcome = self.continue_prepared(p, sink);
        let settled_now = settled(self.dirs, ticket)
            .is_some_and(|kept| kept.comment == typed.comment && kept.at == typed.at);
        match outcome {
            // Refused before the restart was kept or a REPLY posted: a read
            // or a write that failed, tried again at the next pass.
            Err(stop @ Stop::Refused(_)) if !settled_now => {
                unread(ticket, &typed.comment, &stop, sink);
                None
            }
            outcome => Some(outcome),
        }
    }

    /// `stop`, a refusal `continue` would print before a park is restarted,
    /// told by one REPLY when the restart was typed on the ticket (OWL-488).
    pub(super) fn typed_refused(&self, p: &Prepared, stop: Stop, sink: &mut EventSink<'_>) -> Stop {
        let Some(typed) = &p.typed else {
            return stop;
        };
        let reason = match &stop {
            Stop::Refused(reason) => reason.clone(),
            other => other.to_string(),
        };
        let remedy = format!(
            "Once that is settled, reply {} again, in a comment of its own, or run {} on the \
             runner's machine.",
            writer::code("continue"),
            writer::code(&format!("owlshift continue {}", p.ticket))
        );
        let account = p.decider.as_ref().ok().map(|found| found.account.clone());
        self.reply_restart(&p.ticket, account.as_deref(), typed, &reason, &remedy, sink);
        stop
    }

    /// Posts the REPLY to the `continue` comment `typed` whose restart was
    /// refused for `reason`, mentioning `decider` when known, then keeps
    /// that comment as settled. Returns whether it was posted; one that was
    /// not is a `warning`, and the comment is answered at a later pass.
    pub(super) fn reply_restart(
        &self,
        ticket: &TicketId,
        decider: Option<&str>,
        typed: &Typed,
        reason: &str,
        remedy: &str,
        sink: &mut EventSink<'_>,
    ) -> bool {
        let reply = writer::RestartReplyComment {
            ticket: ticket.clone(),
            reason: reason.to_owned(),
            remedy: remedy.to_owned(),
        };
        let mut writer = Writer::new(self.tracker);
        if let Some(account) = decider {
            writer = writer.mentioning(account);
        }
        match writer.post_restart_reply(&reply) {
            Ok(Posted {
                comment: posted,
                mention_dropped,
            }) => {
                sink.emit(
                    ticket,
                    None,
                    EventKind::TrackerWrite,
                    comment_written("REPLY", &posted),
                );
                if let Some(dropped) = &mention_dropped {
                    sink.emit(
                        ticket,
                        None,
                        EventKind::Warning,
                        dropped_mention("REPLY", &posted, dropped),
                    );
                }
                settle(
                    self.dirs,
                    ticket,
                    &typed.comment,
                    typed.at,
                    Settled::Replied,
                    sink,
                );
                true
            }
            Err(error) => {
                sink.emit(
                    ticket,
                    None,
                    EventKind::Warning,
                    data([
                        ("what", json!("restart_reply_unposted")),
                        ("comment", json!(typed.comment)),
                        ("reason", json!(error.to_string())),
                    ]),
                );
                false
            }
        }
    }

    /// Whether the park `state` of `p`, restarted, is one watch takes as
    /// `continue --to-watch` hands it (OWL-420): any but one restarted to
    /// Intake itself.
    pub(super) fn typed_handed(&self, p: &Prepared, state: &TicketState) -> bool {
        let followed = p
            .stored
            .as_ref()
            .and_then(|stored| stored.record.delivery.as_ref())
            .is_some_and(TicketDelivery::followed);
        match state.apply(p.pipeline, Event::Restarted) {
            Ok(Transition::To(next)) => {
                next.status() != Status::Active(Stage::Intake)
                    && to_watch::takes(&next, followed).is_some()
            }
            _ => false,
        }
    }

    /// What the REPLY to a `continue` comment says while the project is
    /// quarantined (OWL-488), from watch's pass, which runs no step then:
    /// the refusal and its remedy.
    pub fn quarantine_reply() -> (String, String) {
        (
            "the project is quarantined on the runner's machine: a run's isolation check did not \
             pass, and no ticket of the project runs until a person clears it"
                .to_owned(),
            format!(
                "A person on that machine looks at what the run changed and runs {}; then reply \
                 {} again, in a comment of its own.",
                writer::code("owlshift unquarantine"),
                writer::code("continue")
            ),
        )
    }

    /// The REPLY of watch's quarantined pass to `comment`, the decider's
    /// `continue` on parked `ticket`.
    pub fn reply_quarantined(
        &self,
        ticket: &TicketId,
        decider: &str,
        comment: &Comment,
        sink: &mut EventSink<'_>,
    ) -> bool {
        let (reason, remedy) = Self::quarantine_reply();
        let typed = Typed::of(comment);
        self.reply_restart(ticket, Some(decider), &typed, &reason, &remedy, sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use owlshift_adapters::tracker::Author as TrackerAuthor;

    fn at(minute: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + minute * 60).unwrap()
    }

    fn comment(id: &str, author: &str, minute: i64, body: &str) -> Comment {
        Comment {
            id: id.to_owned(),
            author: TrackerAuthor::Account(Person {
                id: author.to_owned(),
                name: author.to_owned(),
            }),
            created_at: at(minute),
            edited_at: None,
            body: body.to_owned(),
            parent: None,
            reactions: Vec::new(),
        }
    }

    fn parked(id: &str, minute: i64) -> Comment {
        comment(
            id,
            "owlshift",
            minute,
            "[owlshift] PARKED\n\nParked: blocked.\n\n<!-- owlshift:{\"format\":1,\"kind\":\"PARKED\",\"ticket\":\"DEMO-1\"} -->\n",
        )
    }

    fn decider() -> Person {
        Person {
            id: "dana".to_owned(),
            name: "Dana".to_owned(),
        }
    }

    fn id(found: Option<&Comment>) -> Option<&str> {
        found.map(|comment| comment.id.as_str())
    }

    #[test]
    fn the_word_alone_counts_blanks_and_case_aside() {
        for body in ["continue", "  Continue\n", "CONTINUE", "\tcontinue "] {
            assert!(is_restart_word(body), "{body:?}");
        }
        for body in ["continue.", "/continue", "please continue", "", "cont inue"] {
            assert!(!is_restart_word(body), "{body:?}");
        }
    }

    /// Only the decider's, after the latest PARKED comment, counts; the
    /// newest of theirs is the one.
    #[test]
    fn the_deciders_continue_after_the_latest_park_counts() {
        let decider = decider();
        let thread = vec![
            comment("c1", "dana", 1, "continue"),
            parked("p1", 2),
            comment("c2", "dana", 3, "Continue"),
            comment("c3", "erin", 4, "continue"),
            comment("c4", "dana", 5, "continue, please"),
        ];
        assert_eq!(
            id(restart_comment(&thread, Some(&decider), None)),
            Some("c2")
        );
        // Anyone's, for watch's read: the newest.
        assert_eq!(id(restart_comment(&thread, None, None)), Some("c3"));
        // Another person's alone counts for nothing.
        let others = vec![parked("p1", 1), comment("c1", "erin", 2, "continue")];
        assert_eq!(restart_comment(&others, Some(&decider), None), None);
        // Older than the park, or no park at all: nothing.
        let older = vec![comment("c1", "dana", 1, "continue"), parked("p1", 2)];
        assert_eq!(restart_comment(&older, None, None), None);
        let unparked = vec![comment("c1", "dana", 1, "continue")];
        assert_eq!(restart_comment(&unparked, None, None), None);
    }

    /// A ticket parked again after a restart: the comment that restarted it
    /// counts no more, even when the new PARKED comment was never posted;
    /// a new one after the new park counts.
    #[test]
    fn a_settled_comment_counts_once() {
        let decider = decider();
        let mut thread = vec![parked("p1", 1), comment("c1", "dana", 2, "continue")];
        let kept = TicketRestart {
            format: Format,
            comment: "c1".to_owned(),
            at: at(2),
            settled: Settled::Restarted,
        };
        assert_eq!(restart_comment(&thread, Some(&decider), Some(&kept)), None);
        thread.push(parked("p2", 3));
        assert_eq!(restart_comment(&thread, Some(&decider), Some(&kept)), None);
        thread.push(comment("c2", "dana", 4, "continue"));
        assert_eq!(
            id(restart_comment(&thread, Some(&decider), Some(&kept))),
            Some("c2")
        );
    }
}
