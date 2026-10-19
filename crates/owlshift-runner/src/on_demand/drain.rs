//! The drain's two commands (OWL-250; architecture section 5, "The drain"):
//! `owlshift watch --drain` takes a ticket in through [`OnDemand::intake_seen`]
//! and dispatches one to Build through [`OnDemand::dispatch_seen`], `do`'s own
//! path. Each runs under the project's lock, and only while the ticket ref is
//! still the one watch's snapshot read and the ticket, read again, still
//! shows the ready state and the gesture and carries no exclusion label;
//! otherwise nothing runs and the next pass reads the ticket again. Both
//! check the admission gesture against `[tracker].admitters` at the base
//! commit ([`OnDemand::gesture_for`]): a ticket whose admitter was dropped
//! from the list is taken no further until its gesture, read again, admits it
//! (section 8). The dispatch also reads every ticket ref of the project
//! again under the lock, for the slot and for the open pull request cap
//! (OWL-313): a pull request delivered since the snapshot holds it.

use owlshift_contracts::config::{Admit, Tracker as TrackerConfig};
use owlshift_contracts::drain::Gesture;
use owlshift_core::schedule::holds_slot_or_unknown;

use super::*;
use crate::ticket_ref::text_sha256;

/// What one of the drain's commands did, once it ran at all.
#[derive(Clone, Debug, PartialEq)]
pub enum Taken<T> {
    /// It ran: its outcome, as `do`'s or intake's.
    Ran(Result<T, Stop>),
    /// The gesture the tracker shows was made by `account`, which does not
    /// admit; nothing ran.
    NotAdmitted { account: String, reason: String },
}

/// What the drain's dispatch did, once it ran at all.
#[derive(Clone, Debug, PartialEq)]
pub enum DispatchSeen {
    /// It ran, or its gesture did not admit.
    Taken(Taken<Delivered>),
    /// The ticket refs read under the lock show `open_prs` pull requests
    /// followed, at least `policy.max_open_prs`: no Build started.
    Capped { open_prs: u32, max_open_prs: u32 },
}

/// What the project's ticket refs show, read under the lock.
enum Locked {
    /// Another ticket ref holds the slot, or a ref cannot be read.
    Held,
    /// The slot is free; `open_prs` refs, the dispatched ticket's own
    /// included, have a followed delivery.
    Free { open_prs: u32 },
}

/// Whether watch still follows the delivery of `stored`: what the open pull
/// request cap counts, in watch's snapshot and under the dispatch's lock.
pub(crate) fn delivery_followed(stored: &Stored) -> bool {
    stored
        .record
        .delivery
        .as_ref()
        .is_some_and(|delivery| delivery.followed())
}

/// Why `found`, whose tracker shows the state `shown`, is no longer one the
/// drain takes: out of the ready column, its admission label taken off, or
/// the exclusion label put on. A state gesture is the ready state itself.
pub fn still_ready(found: &Ticket, shown: &str, tracker: &TrackerConfig) -> Result<(), String> {
    if shown != tracker.states.ready {
        return Err(format!(
            "{} shows {shown:?}, no longer the ready state {:?}",
            found.id, tracker.states.ready
        ));
    }
    if let Admit::Label(label) = &tracker.admit
        && !found.labels.contains(label)
    {
        return Err(format!(
            "{} no longer carries the label {label:?}",
            found.id
        ));
    }
    if let Some(exclude) = &tracker.exclude
        && found.labels.contains(exclude)
    {
        return Err(format!(
            "{} carries the exclusion label {exclude:?}",
            found.id
        ));
    }
    Ok(())
}

impl OnDemand<'_> {
    /// Intake for the drain: `None`, with nothing run or recorded, when the
    /// ticket ref is no longer `seen` (`None` for no ref) or the ticket left
    /// the column; otherwise the gesture checked at the base commit, the
    /// drain's `decision` event (`drain` = `intake`), then
    /// [`OnDemand::intake`]'s run.
    pub fn intake_seen(
        &self,
        ticket: &TicketId,
        seen: Option<&str>,
        sink: &mut EventSink<'_>,
    ) -> Option<Taken<TicketIntake>> {
        let mut p = match self.prepare(ticket, Command::Intake) {
            Ok(p) => p,
            Err(stop) => return Some(Taken::Ran(Err(stop))),
        };
        if p.stored.as_ref().map(|stored| stored.commit.as_str()) != seen {
            return None;
        }
        let gesture = match self.drain_checks(&p) {
            Some(Ok(gesture)) => gesture,
            Some(Err(taken)) => return Some(taken),
            None => return None,
        };
        sink.emit(
            ticket,
            None,
            EventKind::Decision,
            data([
                ("drain", json!("intake")),
                ("account", json!(gesture.account)),
            ]),
        );
        Some(Taken::Ran(self.intake_prepared(&mut p, gesture, sink)))
    }

    /// The drain's dispatch to Build, through `do`'s path: `None`, with
    /// nothing run or recorded, when the ticket ref is no longer `seen`, the
    /// intake record no longer holds the ticket's current text, the ticket
    /// left the column, or another ticket ref of the project, read under the
    /// lock, holds the slot (or cannot be read); [`DispatchSeen::Capped`] when
    /// those refs show the open pull request cap reached (OWL-313). Its
    /// `dispatch` event names `command` = `drain`.
    pub fn dispatch_seen(
        &self,
        ticket: &TicketId,
        seen: &str,
        sink: &mut EventSink<'_>,
    ) -> Option<DispatchSeen> {
        let mut p = match self.prepare(ticket, Command::Drain) {
            Ok(p) => p,
            Err(stop) => return Some(DispatchSeen::Taken(Taken::Ran(Err(stop)))),
        };
        if p.stored.as_ref().map(|stored| stored.commit.as_str()) != Some(seen) {
            return None;
        }
        let text = text_sha256(&p.found.title, &p.found.description);
        if p.intake.as_ref().map(|record| record.text_sha256.as_str()) != Some(text.as_str()) {
            return None;
        }
        match self.drain_checks(&p) {
            Some(Ok(_)) => {}
            Some(Err(taken)) => return Some(DispatchSeen::Taken(taken)),
            None => return None,
        }
        // The slot and the cap as the refs show them under this lock: a
        // command typed since the snapshot may have left another ticket at
        // Build, or delivered its pull request.
        let Locked::Free { open_prs } = self.under_lock(&p) else {
            return None;
        };
        let max_open_prs = u32::from(self.config.policy.max_open_prs());
        if open_prs >= max_open_prs {
            return Some(DispatchSeen::Capped {
                open_prs,
                max_open_prs,
            });
        }
        // The dispatch runs the ticket's first stage step (OWL-367): watch
        // resumes it at a later pass.
        p.stepping = true;
        let outcome = self.run_prepared(&mut p, Command::Drain, sink);
        Some(DispatchSeen::Taken(Taken::Ran(outcome)))
    }

    /// What both commands check once the ref is the one seen: the ticket
    /// still in the column ([`still_ready`]), then its gesture at the base.
    /// `None` when it left the column; `Some(Err)` when its stage or the
    /// base cannot be read, or its gesture does not admit.
    fn drain_checks<T>(&self, p: &Prepared) -> Option<Result<Gesture, Taken<T>>> {
        let shown = match self.tracker.stage(&p.ticket) {
            Ok(shown) => shown,
            Err(e) => {
                let stop = refused(&format!("reading the stage of {}", p.ticket), e);
                return Some(Err(Taken::Ran(Err(stop))));
            }
        };
        if still_ready(&p.found, &shown, &self.config.tracker).is_err() {
            return None;
        }
        let admitters = match project::admitters_at(&self.executor.git, &p.checkout, &p.base) {
            Ok(admitters) => admitters,
            Err(why) => return Some(Err(Taken::Ran(Err(Stop::Refused(why))))),
        };
        Some(
            self.gesture_for(p, admitters.as_deref())
                .map_err(|refusal| match refusal.account {
                    Some(account) => Taken::NotAdmitted {
                        account,
                        reason: refusal.reason,
                    },
                    None => Taken::Ran(Err(Stop::Refused(refusal.reason))),
                }),
        )
    }

    /// The project's ticket refs read under the lock `p` holds: the slot is
    /// held when a ref other than `p`'s holds it, or when one cannot be
    /// read, since nothing tells it does not; otherwise the followed
    /// deliveries of every ref, `p`'s own included, as watch's snapshot
    /// counts them. A ref gone since the listing neither holds nor counts.
    fn under_lock(&self, p: &Prepared) -> Locked {
        let git = &self.executor.git;
        let Ok(tickets) = ticket_ref::list(git, &p.checkout) else {
            return Locked::Held;
        };
        let mut open_prs: u32 = 0;
        for ticket in &tickets {
            let own = *ticket == p.ticket;
            let read = if own {
                p.stored.clone()
            } else {
                match ticket_ref::read(git, &p.checkout, ticket) {
                    Ok(read) => read,
                    Err(_) => return Locked::Held,
                }
            };
            let Some(stored) = read else {
                continue;
            };
            if !own
                && holds_slot_or_unknown(TicketState::try_from(&stored.record.state).ok().as_ref())
            {
                return Locked::Held;
            }
            if delivery_followed(&stored) {
                open_prs = open_prs.saturating_add(1);
            }
        }
        Locked::Free { open_prs }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(admit: Admit, exclude: Option<&str>) -> TrackerConfig {
        let text = "kind = \"markdown\"\nadmit = \"delegation\"\n\
                    states = { ready = \"Todo\", working = \"In Progress\", \
                    needs_input = \"Needs Input\", review = \"In Review\" }\n";
        let mut config: TrackerConfig = toml::from_str(text).unwrap();
        config.admit = admit;
        config.exclude = exclude.map(ToOwned::to_owned);
        config
    }

    fn found(labels: &[&str]) -> Ticket {
        Ticket {
            id: TicketId::new("DEMO-1").unwrap(),
            title: "t".to_owned(),
            description: String::new(),
            priority: owlshift_contracts::Priority::Unset,
            assignee: None,
            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
            author: TrackerAuthor::unknown(),
        }
    }

    #[test]
    fn a_ticket_out_of_the_column_unlabelled_or_excluded_is_not_taken() {
        let label = tracker(Admit::Label("agent".to_owned()), Some("hold"));
        assert_eq!(still_ready(&found(&["agent"]), "Todo", &label), Ok(()));
        for (labels, shown, why) in [
            (&["agent"][..], "In Progress", "no longer the ready state"),
            (&[][..], "Todo", "no longer carries the label"),
            (&["agent", "hold"][..], "Todo", "exclusion label"),
        ] {
            let error = still_ready(&found(labels), shown, &label).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        }
        let state = tracker(Admit::State("Todo".to_owned()), None);
        assert_eq!(still_ready(&found(&[]), "Todo", &state), Ok(()));
        assert!(still_ready(&found(&[]), "Backlog", &state).is_err());
    }
}
