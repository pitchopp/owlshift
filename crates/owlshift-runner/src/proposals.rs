//! A run's follow-ups, filed in the tracker's proposals inbox (OWL-253,
//! architecture section 5, "Follow-ups").
//!
//! The follow-ups of a DELIVERY report owed are kept in the ticket ref's
//! `proposals.json` as owed ([`owe`]), in the very write that keeps the
//! delivery or the repair, and from the owed report itself before each
//! filing, which covers a push a watch pass adopted. Before the report is
//! posted, the runner files each owed one ([`file_owed`]) through the Writer
//! and keeps it as filed: a search by its title and fingerprint first, so a
//! filing a crash cut short counts as filed and another ticket's proposal
//! is a duplicate; then, oldest first, at most [`MAX_PER_PARENT`] filed per
//! parent over all its runs. A filing refused stays owed, warned of once
//! until its reason changes, and a later pass files it.

use std::num::NonZeroU32;

use serde_json::json;
use sha2::{Digest, Sha256};

use owlshift_adapters::tracker::{Capability, Found, Tracker};
use owlshift_contracts::drain::{KeptProposal, ProposalOutcome, ProposingRun, TicketProposals};
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{DeliveredPullRequest, OwedReport};
use owlshift_contracts::result::Followup;
use owlshift_core::proposal::normalize_title;

use crate::events::{EventSink, data};
use crate::writer::{ProposalDescription, Proposed, Writer};

/// The most proposals filed per parent ticket, over all its runs.
pub const MAX_PER_PARENT: usize = 3;

/// The most proposals a ticket ref keeps: the follow-ups past it are not
/// owed, and their report says they are not filed.
pub const MAX_KEPT: usize = 64;

/// The most bytes `proposals.json` may take, far below what a ticket ref
/// file holds (1 MiB): a follow-up that would take it past is not owed.
pub const MAX_BYTES: usize = 512 * 1024;

/// The fingerprint of a title: the first 12 lowercase hexadecimal digits
/// of the SHA-256 of its [`normalize_title`].
pub fn fingerprint(title: &str) -> String {
    Sha256::digest(normalize_title(title).as_bytes())
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether the tracker has a proposals inbox: without one nothing is owed,
/// and a report lists its follow-ups as not filed.
pub fn has_inbox(tracker: &dyn Tracker) -> bool {
    tracker.capabilities().contains(&Capability::ProposalsInbox)
}

/// `current` with the follow-ups of each report in `owed` that it does not
/// keep yet, each owed and named after the report's run; `None` when that
/// adds nothing. A correction that pushed nothing owes none, nor a
/// follow-up without a title; a fingerprint already kept is not kept
/// twice, and none is kept past [`MAX_KEPT`] or [`MAX_BYTES`].
pub fn owe<'a>(
    current: Option<&TicketProposals>,
    owed: impl IntoIterator<Item = &'a OwedReport>,
) -> Option<TicketProposals> {
    let mut next = current.cloned().unwrap_or(TicketProposals {
        format: Format,
        proposals: Vec::new(),
    });
    let mut added = false;
    for report in owed {
        let Some(kept) = report.report.as_ref().filter(|_| !report.unchanged) else {
            continue;
        };
        let run = ProposingRun {
            kind: report.kind,
            count: NonZeroU32::new(report.count).unwrap_or(NonZeroU32::MIN),
        };
        for followup in &kept.followups {
            if next.proposals.len() >= MAX_KEPT || normalize_title(&followup.title).is_empty() {
                continue;
            }
            let fingerprint = fingerprint(&followup.title);
            if next.proposals.iter().any(|p| p.fingerprint == fingerprint) {
                continue;
            }
            next.proposals.push(KeptProposal {
                fingerprint,
                proposal: followup.clone(),
                run,
                outcome: ProposalOutcome::Owed { refusal: None },
            });
            if next.render().len() > MAX_BYTES {
                next.proposals.pop();
                continue;
            }
            added = true;
        }
    }
    added.then_some(next)
}

/// The follow-ups of a report, each with what `proposals` keeps of it.
pub fn reported(followups: &[Followup], proposals: Option<&TicketProposals>) -> Vec<Proposed> {
    followups
        .iter()
        .map(|followup| {
            let fingerprint = fingerprint(&followup.title);
            let outcome = proposals.and_then(|kept| {
                kept.proposals
                    .iter()
                    .find(|p| p.fingerprint == fingerprint)
                    .map(|p| p.outcome.clone())
            });
            Proposed {
                followup: followup.clone(),
                outcome,
            }
        })
        .collect()
}

/// What the search before a filing settles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settled {
    /// A ticket whose footer names this parent and fingerprint: a filing a
    /// crash cut short before it was kept.
    Filed(TicketId),
    /// Another ticket of the team not completed, with the same title or
    /// the fingerprint: already proposed.
    Duplicate(TicketId),
    /// Nothing found: to be filed.
    Unfiled,
}

/// Settles a proposal of `parent` with `fingerprint` by what the search
/// found ([`Tracker::proposed`]): a ticket whose footer names both is the
/// one filed, else the first ticket found is a duplicate.
pub fn settle(parent: &TicketId, fingerprint: &str, found: &[Found]) -> Settled {
    let ours = found.iter().find(|found| {
        ProposalDescription::footer_of(&found.description)
            .is_some_and(|(p, f)| p == parent.as_str() && f == fingerprint)
    });
    match (ours, found.first()) {
        (Some(ours), _) => Settled::Filed(ours.id.clone()),
        (None, Some(other)) => Settled::Duplicate(other.id.clone()),
        (None, None) => Settled::Unfiled,
    }
}

/// Where a parent's proposals are filed, from the project's configuration.
pub struct Filing<'a> {
    pub tracker: &'a dyn Tracker,
    /// `[tracker].team`.
    pub team: Option<&'a str>,
    /// `[tracker].states.ready`.
    pub ready: &'a str,
    /// The `[tracker].admit` label, `None` for another gesture.
    pub admission_label: Option<&'a str>,
    pub parent: &'a TicketId,
    pub pull_request: Option<&'a DeliveredPullRequest>,
}

/// Files the proposals `proposals` owes, oldest first, and returns whether
/// any outcome or refusal changed, for the caller to keep. Nothing is tried
/// on a tracker without a proposals inbox.
///
/// Every owed proposal is searched first: one found filed or proposed is
/// settled so, before any room is counted, so a filing a crash cut short
/// takes its place among the three. A search that fails keeps its proposal
/// owed, and the room it may hold: nothing is filed into it. The rest are
/// filed while fewer than [`MAX_PER_PARENT`] are filed, and past that are
/// over the limit. A refusal keeps the proposal owed with its reason, a
/// `proposal_unfiled` warning when the reason is new; what went wrong once
/// a ticket was filed is a `proposal_incomplete` warning each, and the
/// proposal counts as filed. A relation not written then is never written
/// again: the description's "Blocked by" line holds the proposal once a
/// person promotes it, and its intake reads the parent as a dependency,
/// which part F relates while the parent is not done (OWL-268,
/// architecture section 5, "A relation lost after filing").
pub fn file_owed(
    filing: &Filing<'_>,
    proposals: &mut TicketProposals,
    sink: &mut EventSink<'_>,
) -> bool {
    if !has_inbox(filing.tracker) {
        return false;
    }
    let parent = filing.parent;
    let mut changed = false;
    let mut unfiled = Vec::new();
    let mut unsure = 0;
    for (i, kept) in proposals.proposals.iter_mut().enumerate() {
        if !matches!(kept.outcome, ProposalOutcome::Owed { .. }) {
            continue;
        }
        let title = ProposalDescription {
            parent: parent.clone(),
            pull_request: None,
            run: kept.run.kind,
            followup: kept.proposal.clone(),
            fingerprint: kept.fingerprint.clone(),
        }
        .title();
        let marker = ProposalDescription::marker(&kept.fingerprint);
        match filing.tracker.proposed(filing.team, &title, &marker) {
            Err(error) => {
                unsure += 1;
                changed |= refused(parent, kept, error.to_string(), sink);
            }
            Ok(found) => match settle(parent, &kept.fingerprint, &found) {
                Settled::Filed(ticket) => {
                    settled(parent, kept, ProposalOutcome::Filed { ticket }, sink);
                    changed = true;
                }
                Settled::Duplicate(ticket) => {
                    settled(parent, kept, ProposalOutcome::Duplicate { ticket }, sink);
                    changed = true;
                }
                Settled::Unfiled => unfiled.push(i),
            },
        }
    }
    let writer = Writer::new(filing.tracker);
    for i in unfiled {
        let filed = proposals
            .proposals
            .iter()
            .filter(|p| matches!(p.outcome, ProposalOutcome::Filed { .. }))
            .count();
        let kept = &mut proposals.proposals[i];
        if filed >= MAX_PER_PARENT {
            settled(parent, kept, ProposalOutcome::OverLimit, sink);
            changed = true;
            continue;
        }
        if filed + unsure >= MAX_PER_PARENT {
            continue;
        }
        let description = ProposalDescription {
            parent: parent.clone(),
            pull_request: filing.pull_request.cloned(),
            run: kept.run.kind,
            followup: kept.proposal.clone(),
            fingerprint: kept.fingerprint.clone(),
        };
        match writer.propose(&description, filing.ready, filing.admission_label) {
            Ok(done) => {
                sink.emit(
                    parent,
                    None,
                    EventKind::TrackerWrite,
                    data([
                        ("target", json!("tracker")),
                        ("action", json!("propose")),
                        ("ticket", json!(done.id.as_str())),
                        ("state", json!(done.state)),
                        ("fingerprint", json!(kept.fingerprint)),
                    ]),
                );
                for problem in &done.problems {
                    sink.emit(
                        parent,
                        None,
                        EventKind::Warning,
                        data([
                            ("what", json!("proposal_incomplete")),
                            ("ticket", json!(done.id.as_str())),
                            ("fingerprint", json!(kept.fingerprint)),
                            ("reason", json!(problem.to_string())),
                        ]),
                    );
                }
                settled(
                    parent,
                    kept,
                    ProposalOutcome::Filed { ticket: done.id },
                    sink,
                );
                changed = true;
            }
            Err(error) => changed |= refused(parent, kept, error.to_string(), sink),
        }
    }
    changed
}

/// Keeps `outcome` and tells it: a `decision` event `drain` = `proposal`.
fn settled(
    parent: &TicketId,
    kept: &mut KeptProposal,
    outcome: ProposalOutcome,
    sink: &mut EventSink<'_>,
) {
    let (name, ticket) = match &outcome {
        ProposalOutcome::Filed { ticket } => ("filed", Some(ticket.as_str())),
        ProposalOutcome::Duplicate { ticket } => ("duplicate", Some(ticket.as_str())),
        ProposalOutcome::OverLimit => ("over_limit", None),
        ProposalOutcome::Owed { .. } => ("owed", None),
    };
    sink.emit(
        parent,
        None,
        EventKind::Decision,
        data([
            ("drain", json!("proposal")),
            ("outcome", json!(name)),
            ("ticket", json!(ticket)),
            ("fingerprint", json!(kept.fingerprint)),
            ("title", json!(kept.proposal.title)),
        ]),
    );
    kept.outcome = outcome;
}

/// Keeps the proposal owed with `reason`; a `proposal_unfiled` warning and
/// `true` when the reason differs from the one kept.
fn refused(
    parent: &TicketId,
    kept: &mut KeptProposal,
    reason: String,
    sink: &mut EventSink<'_>,
) -> bool {
    if matches!(&kept.outcome, ProposalOutcome::Owed { refusal: Some(kept) } if *kept == reason) {
        return false;
    }
    sink.emit(
        parent,
        None,
        EventKind::Warning,
        data([
            ("what", json!("proposal_unfiled")),
            ("fingerprint", json!(kept.fingerprint)),
            ("title", json!(kept.proposal.title)),
            ("reason", json!(reason)),
        ]),
    );
    kept.outcome = ProposalOutcome::Owed {
        refusal: Some(reason),
    };
    true
}

/// Whether `proposals` holds one still owed.
pub fn owes(proposals: Option<&TicketProposals>) -> bool {
    proposals.is_some_and(|kept| {
        kept.proposals
            .iter()
            .any(|p| matches!(p.outcome, ProposalOutcome::Owed { .. }))
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::fs;
    use std::rc::Rc;

    use owlshift_adapters::tracker::{
        Comment, Error as TrackerError, ErrorKind, Filed, Proposal, Ticket,
    };
    use owlshift_contracts::event::Event;
    use owlshift_contracts::refs::{KeptReport, PushKind};
    use owlshift_contracts::result::FollowupSource;

    use super::*;
    use crate::events::EventLog;

    fn followup(title: &str) -> Followup {
        Followup {
            title: title.to_owned(),
            why: "Because.".to_owned(),
            evidence: vec!["src/lib.rs:1".to_owned()],
            source: FollowupSource::Agent,
            done_when: "Done.".to_owned(),
            blocked_by_parent: false,
        }
    }

    fn report(kind: PushKind, count: u32, titles: &[&str]) -> OwedReport {
        OwedReport {
            kind,
            count,
            report: Some(KeptReport {
                summary: "s".to_owned(),
                gate: None,
                checks: vec![],
                questions: vec![],
                notes: vec![],
                followups: titles.iter().map(|t| followup(t)).collect(),
                reviews: Vec::new(),
            }),
            unchanged: false,
        }
    }

    fn id(s: &str) -> TicketId {
        TicketId::new(s).unwrap()
    }

    fn found(ticket: &str, description: &str) -> Found {
        Found {
            id: id(ticket),
            description: description.to_owned(),
        }
    }

    type Search = Box<dyn Fn(&str) -> Result<Vec<Found>, TrackerError>>;
    type File = Box<dyn Fn(&str) -> Result<Filed, TrackerError>>;

    /// A tracker with a proposals inbox: each search answers what `search`
    /// gives for the title, each filing what `file` gives.
    struct Inbox {
        inbox: bool,
        search: Search,
        file: File,
        filed: RefCell<Vec<String>>,
    }

    impl Inbox {
        fn new(
            search: impl Fn(&str) -> Result<Vec<Found>, TrackerError> + 'static,
            file: impl Fn(&str) -> Result<Filed, TrackerError> + 'static,
        ) -> Self {
            Self {
                inbox: true,
                search: Box::new(search),
                file: Box::new(file),
                filed: RefCell::new(vec![]),
            }
        }
    }

    fn unused() -> TrackerError {
        TrackerError::new(ErrorKind::Other, "unused")
    }

    impl Tracker for Inbox {
        fn capabilities(&self) -> &'static [Capability] {
            if self.inbox {
                &[Capability::ProposalsInbox]
            } else {
                &[]
            }
        }
        fn ticket(&self, _: &TicketId) -> Result<Ticket, TrackerError> {
            Err(unused())
        }
        fn comments(&self, _: &TicketId) -> Result<Vec<Comment>, TrackerError> {
            Err(unused())
        }
        fn post_comment(&self, _: &TicketId, _: &str) -> Result<Comment, TrackerError> {
            Err(unused())
        }
        fn set_stage(&self, _: &TicketId, _: &str) -> Result<(), TrackerError> {
            Err(unused())
        }
        fn propose(&self, proposal: &Proposal<'_>) -> Result<Filed, TrackerError> {
            assert_eq!(proposal.ready, "Todo");
            self.filed
                .borrow_mut()
                .push(proposal.description.to_owned());
            (self.file)(proposal.title)
        }
        fn proposed(
            &self,
            team: Option<&str>,
            title: &str,
            marker: &str,
        ) -> Result<Vec<Found>, TrackerError> {
            assert_eq!(team, Some("OWL"));
            assert_eq!(marker, ProposalDescription::marker(&fingerprint(title)));
            (self.search)(title)
        }
    }

    fn filed_as(ticket: &str) -> Result<Filed, TrackerError> {
        Ok(Filed {
            id: id(ticket),
            state: "Triage".to_owned(),
            problems: vec![],
        })
    }

    /// Files what `proposals` owes on `tracker` and returns whether it
    /// changed, with the events emitted.
    fn file(
        tracker: &dyn Tracker,
        admission_label: Option<&str>,
        proposals: &mut TicketProposals,
    ) -> (bool, Vec<Event>) {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::in_dir(dir.path());
        let mut out = Vec::new();
        let parent = id("OWL-7");
        let pull_request = DeliveredPullRequest {
            number: 12,
            url: "https://github.com/o/r/pull/12".to_owned(),
        };
        let filing = Filing {
            tracker,
            team: Some("OWL"),
            ready: "Todo",
            admission_label,
            parent: &parent,
            pull_request: Some(&pull_request),
        };
        let changed = file_owed(
            &filing,
            proposals,
            &mut EventSink::new("p", log.clone(), &mut out),
        );
        let events = fs::read_to_string(log.path())
            .unwrap_or_default()
            .lines()
            .map(|line| Event::parse(line).unwrap())
            .collect();
        (changed, events)
    }

    fn owed(titles: &[&str]) -> TicketProposals {
        owe(None, [&report(PushKind::Delivery, 1, titles)]).unwrap()
    }

    fn outcomes(proposals: &TicketProposals) -> Vec<ProposalOutcome> {
        proposals
            .proposals
            .iter()
            .map(|p| p.outcome.clone())
            .collect()
    }

    fn what(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .map(|e| {
                let name = e.data.get("what").or_else(|| e.data.get("outcome"));
                format!(
                    "{:?}:{}",
                    e.kind,
                    name.and_then(|v| v.as_str()).unwrap_or("-")
                )
            })
            .collect()
    }

    #[test]
    fn the_fingerprint_reads_the_normalized_title() {
        // The first 12 digits of the SHA-256 of "cap the retries".
        assert_eq!(fingerprint("Cap the retries"), "647f4576ff92");
        assert_eq!(fingerprint("  cap-the RETRIES. "), "647f4576ff92");
        assert_ne!(fingerprint("Cap the retry"), "647f4576ff92");
    }

    #[test]
    fn a_runs_follow_ups_are_owed_once_by_fingerprint() {
        let first = owe(
            None,
            [&report(PushKind::Delivery, 1, &["Cap it", "cap it!", " "])],
        )
        .unwrap();
        // The same fingerprint once; no title, no proposal.
        assert_eq!(first.proposals.len(), 1);
        assert_eq!(first.proposals[0].run.kind, PushKind::Delivery);
        assert_eq!(
            first.proposals[0].outcome,
            ProposalOutcome::Owed { refusal: None }
        );
        // A later run's: only what is new, named after it.
        let next = owe(
            Some(&first),
            [&report(PushKind::Fix, 2, &["Cap it", "Log it"])],
        )
        .unwrap();
        assert_eq!(next.proposals.len(), 2);
        assert_eq!(
            (
                next.proposals[1].run.kind,
                next.proposals[1].run.count.get()
            ),
            (PushKind::Fix, 2)
        );
        assert_eq!(
            owe(Some(&next), [&report(PushKind::Fix, 3, &["log  IT"])]),
            None
        );
        // A correction that pushed nothing owes none.
        let mut unchanged = report(PushKind::Correction, 1, &["New one"]);
        unchanged.unchanged = true;
        assert_eq!(owe(Some(&next), [&unchanged]), None);
        // Bounded in count and in bytes.
        let many: Vec<String> = (0..MAX_KEPT + 5)
            .map(|n| format!("Follow-up {n}"))
            .collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(owed(&many).proposals.len(), MAX_KEPT);
        let mut big = report(PushKind::Delivery, 1, &["Big one", "Small one"]);
        big.report.as_mut().unwrap().followups[0].why = "w".repeat(MAX_BYTES);
        let kept = owe(None, [&big]).unwrap();
        assert_eq!(kept.proposals.len(), 1);
        assert_eq!(kept.proposals[0].proposal.title, "Small one");
    }

    #[test]
    fn the_search_settles_our_filing_or_a_duplicate() {
        let parent = id("OWL-7");
        let ours = ProposalDescription::footer(&parent, "0123456789ab");
        let theirs = ProposalDescription::footer(&id("OWL-8"), "0123456789ab");
        for (hits, settled) in [
            (vec![], Settled::Unfiled),
            (
                vec![
                    found("OWL-20", "x"),
                    found("OWL-21", &format!("Why.\n\n{ours}\n")),
                ],
                Settled::Filed(id("OWL-21")),
            ),
            // The same fingerprint from another parent, or the same title.
            (
                vec![found("OWL-22", &theirs)],
                Settled::Duplicate(id("OWL-22")),
            ),
            (vec![found("OWL-23", "")], Settled::Duplicate(id("OWL-23"))),
            // A footer that is not the last line is not ours.
            (
                vec![found("OWL-24", &format!("{ours}\nmore"))],
                Settled::Duplicate(id("OWL-24")),
            ),
        ] {
            assert_eq!(settle(&parent, "0123456789ab", &hits), settled, "{hits:?}");
        }
    }

    #[test]
    fn a_filing_is_found_again_and_counts_before_any_room() {
        // Two filed, then X filed by a pass a crash cut short, then Y: X is
        // found as ours and takes the third place, so Y is over the limit.
        let mut proposals = owed(&["A", "B", "X", "Y"]);
        proposals.proposals[0].outcome = ProposalOutcome::Filed {
            ticket: id("OWL-30"),
        };
        proposals.proposals[1].outcome = ProposalOutcome::Filed {
            ticket: id("OWL-31"),
        };
        let x = proposals.proposals[2].fingerprint.clone();
        let tracker = Inbox::new(
            move |title| {
                Ok(if title == "X" {
                    let footer = ProposalDescription::footer(&id("OWL-7"), &x);
                    vec![found("OWL-32", &format!("Why.\n\n{footer}\n"))]
                } else {
                    vec![]
                })
            },
            |_| panic!("nothing is filed"),
        );
        let (changed, events) = file(&tracker, None, &mut proposals);
        assert!(changed);
        assert_eq!(
            outcomes(&proposals)[2..],
            [
                ProposalOutcome::Filed {
                    ticket: id("OWL-32")
                },
                ProposalOutcome::OverLimit
            ]
        );
        assert_eq!(what(&events), ["Decision:filed", "Decision:over_limit"]);
    }

    #[test]
    fn a_duplicate_takes_no_room_and_a_search_that_fails_holds_its_own() {
        // Z is filed; A is a duplicate; B is filed; C's search fails, so it
        // may be filed already and holds the last place: D waits, owed.
        let mut proposals = owed(&["Z", "A", "B", "C", "D"]);
        proposals.proposals[0].outcome = ProposalOutcome::Filed {
            ticket: id("OWL-40"),
        };
        let tracker = Inbox::new(
            |title| match title {
                "A" => Ok(vec![found("OWL-41", "")]),
                "C" => Err(TrackerError::new(ErrorKind::Other, "Linear is down")),
                _ => Ok(vec![]),
            },
            |_| filed_as("OWL-42"),
        );
        let (changed, events) = file(&tracker, None, &mut proposals);
        assert!(changed);
        assert_eq!(
            outcomes(&proposals)[1..],
            [
                ProposalOutcome::Duplicate {
                    ticket: id("OWL-41")
                },
                ProposalOutcome::Filed {
                    ticket: id("OWL-42")
                },
                ProposalOutcome::Owed {
                    refusal: Some("Linear is down".to_owned())
                },
                ProposalOutcome::Owed { refusal: None },
            ]
        );
        assert_eq!(
            what(&events),
            [
                "Decision:duplicate",
                "Warning:proposal_unfiled",
                "TrackerWrite:-",
                "Decision:filed"
            ]
        );
        // What was filed carries its parent's pull request and its footer.
        let description = tracker.filed.borrow()[0].clone();
        assert!(
            description.contains("[#12](https://github.com/o/r/pull/12)"),
            "{description}"
        );
        assert_eq!(
            ProposalDescription::footer_of(&description),
            Some(("OWL-7".to_owned(), fingerprint("B")))
        );
    }

    #[test]
    fn a_refusal_is_warned_of_once_until_its_reason_changes() {
        let mut proposals = owed(&["A"]);
        let reason = Rc::new(RefCell::new("the plan holds 250 issues".to_owned()));
        let said = reason.clone();
        let tracker = Inbox::new(
            |_| Ok(vec![]),
            move |_| Err(TrackerError::new(ErrorKind::Other, said.borrow().clone())),
        );
        let (changed, events) = file(&tracker, None, &mut proposals);
        assert!(changed);
        assert_eq!(what(&events), ["Warning:proposal_unfiled"]);
        assert_eq!(
            outcomes(&proposals),
            [ProposalOutcome::Owed {
                refusal: Some("tracker: the plan holds 250 issues".to_owned())
            }]
        );
        let (changed, events) = file(&tracker, None, &mut proposals);
        assert!(!changed);
        assert!(events.is_empty(), "{events:?}");
        *reason.borrow_mut() = "Linear is down".to_owned();
        let (changed, events) = file(&tracker, None, &mut proposals);
        assert!(changed);
        assert_eq!(what(&events), ["Warning:proposal_unfiled"]);
    }

    #[test]
    fn what_went_wrong_after_a_filing_is_warned_of() {
        let mut proposals = owed(&["A"]);
        let tracker = Inbox::new(
            |_| Ok(vec![]),
            |_| {
                Ok(Filed {
                    id: id("OWL-50"),
                    state: "Triage".to_owned(),
                    problems: vec![TrackerError::new(ErrorKind::Other, "no relation")],
                })
            },
        );
        let (_, events) = file(&tracker, None, &mut proposals);
        assert_eq!(
            what(&events),
            [
                "TrackerWrite:-",
                "Warning:proposal_incomplete",
                "Decision:filed"
            ]
        );
        assert_eq!(
            outcomes(&proposals),
            [ProposalOutcome::Filed {
                ticket: id("OWL-50")
            }]
        );
    }

    #[test]
    fn the_floor_refuses_a_proposal_carrying_the_admission_label() {
        let mut proposals = owed(&["A"]);
        let tracker = Inbox::new(|_| Ok(vec![]), |_| panic!("the floor refuses first"));
        let (changed, _) = file(&tracker, Some("Agent:Proposed"), &mut proposals);
        assert!(changed);
        let ProposalOutcome::Owed {
            refusal: Some(refusal),
        } = &proposals.proposals[0].outcome
        else {
            panic!("{proposals:?}");
        };
        assert!(refusal.contains("refused by the policy floor"), "{refusal}");
        assert!(tracker.filed.borrow().is_empty());
    }

    #[test]
    fn a_tracker_without_an_inbox_is_never_asked() {
        let mut proposals = owed(&["A"]);
        let mut tracker = Inbox::new(|_| panic!("no search"), |_| panic!("no filing"));
        tracker.inbox = false;
        let before = proposals.clone();
        assert_eq!(file(&tracker, None, &mut proposals), (false, vec![]));
        assert_eq!(proposals, before);
    }
}
