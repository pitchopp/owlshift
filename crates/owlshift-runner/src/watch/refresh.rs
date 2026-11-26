//! Watch's refresh of an overlap (OWL-449; architecture section 5,
//! "Tolerated overlap", "The refresh"): at each pass, a followed pull
//! request whose ticket shares a path with another Owlshift ticket, by
//! either ticket's record, and whose base head now changes one of those
//! paths, which its head does not contain, is due a refresh rebase, the
//! forge reporting a conflict or not. The base head is read from the
//! forge, once a pass per base branch as merge on green reads it; the
//! paths from git in the dedicated checkout, fetched under a try of the
//! project's lock when it lacks that head; the verdict kept per pushed
//! head and base head. Since OWL-654 the refresh is owed, and runs only
//! once the pull request could otherwise merge, as merge on green decides
//! (`owlshift_core::follow::overlap_refresh`, from [`Watch::merge`]); a
//! repair reads it again the same way ([`held::base_moved_on`]). While it
//! is owed and held, it is told ([`Watch::tell_refresh`]).

use std::collections::HashMap;
use std::io::Write;

use owlshift_contracts::config::Claims;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{DeliveredPullRequest, TicketDelivery};
use owlshift_core::state::Status;
use owlshift_core::vocab::Overlap;

use super::{Memory, Watch};
use crate::events::{Data, EventSink};
use crate::held;
use crate::merge;
use crate::on_demand::delivery_followed;
use crate::project::{self, Base, DefaultBranchFile};
use crate::ticket_ref::RefRead;

/// How many commits' switches [`Refreshes`] keeps.
const SWITCHES_KEPT: usize = 16;

/// What watch keeps of the overlaps' refreshes.
#[derive(Debug, Default)]
pub(super) struct Refreshes {
    /// This pass's: the base head each ticket's refresh rebases onto.
    due: HashMap<TicketId, String>,
    /// Each ticket's last verdict: its pushed head, the base head, what its
    /// overlaps reached then, and whether that base changes it. A taking
    /// recorded since (OWL-621) reaches more on the same heads.
    checked: HashMap<TicketId, (String, String, held::Reach, bool)>,
    /// `policy.overlap` as the project file reads at a commit, the latest
    /// few: a commit's file never changes.
    switches: Vec<(String, Overlap)>,
    /// The owed refresh's wait last told for each ticket (OWL-654): its
    /// event, told again only once it differs. Dropped once no refresh is
    /// due, so a later owed refresh is told even with the same data.
    told: HashMap<TicketId, Data>,
}

impl Refreshes {
    /// The base head `ticket`'s refresh rebases onto at this pass, if due.
    pub(super) fn due(&self, ticket: &TicketId) -> Option<String> {
        self.due.get(ticket).cloned()
    }
}

/// Whether `delivery`'s last rebase was pushed onto `base`: no refresh is
/// owed onto that head again (OWL-449). Watch's refresh and a repair's read
/// of it both skip it (OWL-654).
pub(crate) fn rebased_onto(delivery: &TicketDelivery, base: &str) -> bool {
    delivery.rebased_onto.as_deref() == Some(base)
}

impl Watch<'_> {
    /// Tells `wait`, an owed refresh's `refresh_owed` reason and fields
    /// ([`merge::refresh_wait`], [`merge::refresh_owed`]), for `ticket`'s
    /// pull request `pr` at `head` (OWL-654; architecture section 5,
    /// "Tolerated overlap", "The refresh"): at the first pass that finds it,
    /// once per distinct event (head, base head it would rebase onto, what
    /// it waits for), one `decision` event (`watch` `waits`) and one line,
    /// in the words `owlshift why` shows it with. Kept apart from merge on
    /// green's memory, which a ticket watch reads only for an end forgets at
    /// each look.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn tell_refresh(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        pr: &DeliveredPullRequest,
        head: &str,
        (reason, fields): (&'static str, Data),
    ) {
        let event = merge::wait_data(pr.number, head, reason, fields);
        if memory.refreshes.told.get(ticket) == Some(&event) {
            return;
        }
        memory.refreshes.told.insert(ticket.clone(), event.clone());
        let words = merge::wait_words(reason, &event).unwrap_or_else(|| reason.replace('_', " "));
        sink.emit(ticket, None, EventKind::Decision, event);
        self.line(out, &format!("{ticket}: {words}"));
    }

    /// Tells the refresh owed to `ticket`, which watch reads only for an end
    /// in `status` (OWL-654): its round waiting for its decider, a park, or
    /// its review loop working at Build or Verify ([`merge::held_until`]).
    /// No forge read: whether a refresh is owed is read from git, at this
    /// pass's [`Self::overlap_refreshes`].
    pub(super) fn tell_held_refresh(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        delivery: &TicketDelivery,
        status: Status,
    ) {
        let (Some(onto), Some(until)) = (memory.refreshes.due(ticket), merge::held_until(status))
        else {
            return;
        };
        let (Some(pr), Some(pushed)) = (&delivery.pull_request, &delivery.pushed) else {
            return;
        };
        let wait = merge::refresh_owed(&onto, until, None);
        self.tell_refresh(memory, sink, out, ticket, pr, pushed, wait);
    }

    /// The project's `policy.overlap` at this pass (OWL-449), read at the
    /// default branch's head as the forge gives it, as `[forge] merge` is
    /// read: the project file at that commit in the dedicated checkout,
    /// fetched under a try of the project's lock when it lacks it, kept per
    /// commit. When the forge or that commit cannot be read, the file at
    /// `origin/HEAD` as the checkout last fetched it; `exclusive` when none
    /// reads. Each step reads it again at the base it fetched.
    pub(super) fn overlap_switch(&self, memory: &mut Memory) -> Overlap {
        let on_demand = self.on_demand;
        let git = &on_demand.executor.git;
        let checkout = on_demand.dirs.checkout();
        let Ok(DefaultBranchFile::Read { base, file }) =
            project::project_file_at_origin_head(git, &checkout)
        else {
            return Overlap::Exclusive;
        };
        let fetched = file
            .map(|config| config.policy.overlap())
            .unwrap_or_default();
        // The forge's reads wait for a rate limit's reset (OWL-209).
        if memory.following.paused((on_demand.clock)()) {
            return fetched;
        }
        let Ok(head) = self.base_head(memory, &base.branch) else {
            return fetched;
        };
        if head == base.commit {
            return fetched;
        }
        if let Some((_, switch)) = memory.refreshes.switches.iter().find(|(at, _)| *at == head) {
            return *switch;
        }
        if !self.holds_commit(&head) {
            return fetched;
        }
        let at = Base {
            commit: head.clone(),
            ..base
        };
        let Ok(switch) = project::overlap_at(git, &checkout, &at) else {
            return fetched;
        };
        let switches = &mut memory.refreshes.switches;
        switches.push((head, switch));
        let over = switches.len().saturating_sub(SWITCHES_KEPT);
        switches.drain(..over);
        switch
    }

    /// Reads, for every ticket of `read` whose delivery watch follows and
    /// whose ticket has an overlap, whether its refresh is due at this pass
    /// (module documentation). A base head or a commit that cannot be read,
    /// or a project busy when a fetch is needed, makes none due: the next
    /// pass reads it again.
    pub(super) fn overlap_refreshes(&self, memory: &mut Memory, read: &[RefRead]) {
        memory.refreshes.due.clear();
        // Under remote claims a sibling only the ticket's own record names,
        // another runner's (OWL-587), counts whether its claim still stands
        // or not: its holder deletes it a pass after reading its merge,
        // which a runner asleep then would never see (OWL-613).
        let claims = if memory.claimed.remote {
            Claims::Remote
        } else {
            Claims::Local
        };
        let on_demand = self.on_demand;
        let git = &on_demand.executor.git;
        let checkout = on_demand.dirs.checkout();
        let named = on_demand.config.stack.named_resources().unwrap_or_default();
        for (ticket, stored) in read
            .iter()
            .filter_map(|(ticket, stored)| stored.as_ref().ok().map(|stored| (ticket, stored)))
        {
            let Some(delivery) = stored.record.delivery.as_ref() else {
                continue;
            };
            let Some(pushed) = delivery.pushed.as_deref() else {
                continue;
            };
            if !delivery_followed(stored) {
                continue;
            }
            // The named resources a taking recorded reach their patterns'
            // paths (OWL-621).
            let reach = held::Reach::of(
                &held::sharings_of(ticket, stored.record.held.as_ref(), read, claims),
                &named,
            );
            if reach.is_empty() {
                memory.refreshes.checked.remove(ticket);
                continue;
            }
            let Ok(base) = self.base_head(memory, &delivery.base_branch) else {
                continue;
            };
            if rebased_onto(delivery, &base) {
                continue;
            }
            let known = memory
                .refreshes
                .checked
                .get(ticket)
                .filter(|(at_push, at_base, reached, _)| {
                    at_push == pushed && *at_base == base && *reached == reach
                })
                .map(|(_, _, _, due)| *due);
            let due = match known {
                Some(due) => due,
                None => {
                    if !self.holds_commit(&base) {
                        continue;
                    }
                    let Ok(due) = held::base_moved_on(
                        git,
                        &checkout,
                        pushed,
                        delivery.base_at_push.as_deref(),
                        &base,
                        &reach,
                    ) else {
                        continue;
                    };
                    memory.refreshes.checked.insert(
                        ticket.clone(),
                        (pushed.to_owned(), base.clone(), reach, due),
                    );
                    due
                }
            };
            if due {
                memory.refreshes.due.insert(ticket.clone(), base);
            }
        }
        let refreshes = &mut memory.refreshes;
        refreshes
            .told
            .retain(|ticket, _| refreshes.due.contains_key(ticket));
    }

    /// Whether the dedicated checkout holds `commit`, fetched under a try
    /// of the project's lock when it does not yet: false when it still does
    /// not, the project is busy, or the fetch fails.
    fn holds_commit(&self, commit: &str) -> bool {
        let on_demand = self.on_demand;
        let git = &on_demand.executor.git;
        let checkout = on_demand.dirs.checkout();
        let object = format!("{commit}^{{commit}}");
        let held = || {
            git.output(&checkout, &["cat-file", "-e", &object], None)
                .is_ok_and(|out| out.success())
        };
        if held() {
            return true;
        }
        let Ok(Some(_lock)) = on_demand.dirs.try_lock() else {
            return false;
        };
        project::sync_checkout(git, on_demand.dirs, on_demand.remote_url).is_ok() && held()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OWL-654: a refresh rebase pushed onto a base head is never owed onto
    /// that head again; any other head may be.
    #[test]
    fn a_rebase_onto_a_head_is_never_asked_again_onto_that_head() {
        let mut delivery = TicketDelivery::new("owl-1-a", "main");
        assert!(!rebased_onto(&delivery, "b1"));
        delivery.rebased_onto = Some("b1".to_owned());
        assert!(rebased_onto(&delivery, "b1"));
        assert!(!rebased_onto(&delivery, "b2"));
    }
}
