//! Watch's refresh of an overlap (OWL-449; architecture section 5,
//! "Tolerated overlap", "The refresh"): at each pass, a followed pull
//! request whose ticket shares a path with another Owlshift ticket, by
//! either ticket's record, and whose base head now changes one of those
//! paths, which its head does not contain, is due a refresh rebase, the
//! forge reporting a conflict or not. The base head is read from the
//! forge, once a pass per base branch as merge on green reads it; the
//! paths from git in the dedicated checkout, fetched under a try of the
//! project's lock when it lacks that head; the verdict kept per pushed
//! head and base head. The core's table decides on it
//! (`owlshift_core::follow::Followed::overlap`), and a repair reads it
//! again the same way ([`held::base_moved_on`]).

use std::collections::HashMap;

use owlshift_contracts::ids::TicketId;
use owlshift_core::vocab::Overlap;

use super::{Memory, Watch};
use crate::held;
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
    /// Each ticket's last verdict: its pushed head, the base head, and
    /// whether that base changes a path its overlaps name.
    checked: HashMap<TicketId, (String, String, bool)>,
    /// `policy.overlap` as the project file reads at a commit, the latest
    /// few: a commit's file never changes.
    switches: Vec<(String, Overlap)>,
}

impl Refreshes {
    /// The base head `ticket`'s refresh rebases onto at this pass, if due.
    pub(super) fn due(&self, ticket: &TicketId) -> Option<String> {
        self.due.get(ticket).cloned()
    }
}

impl Watch<'_> {
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
        let on_demand = self.on_demand;
        let git = &on_demand.executor.git;
        let checkout = on_demand.dirs.checkout();
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
            let paths: Vec<String> = held::overlaps_of(ticket, stored.record.held.as_ref(), read)
                .into_iter()
                .flat_map(|(_, paths)| paths)
                .collect();
            if paths.is_empty() {
                memory.refreshes.checked.remove(ticket);
                continue;
            }
            let Ok(base) = self.base_head(memory, &delivery.base_branch) else {
                continue;
            };
            let known = memory
                .refreshes
                .checked
                .get(ticket)
                .filter(|(at_push, at_base, _)| at_push == pushed && *at_base == base)
                .map(|(_, _, due)| *due);
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
                        &paths,
                    ) else {
                        continue;
                    };
                    memory
                        .refreshes
                        .checked
                        .insert(ticket.clone(), (pushed.to_owned(), base.clone(), due));
                    due
                }
            };
            if due {
                memory.refreshes.due.insert(ticket.clone(), base);
            }
        }
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
