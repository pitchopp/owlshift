//! The tracker conformance suite: what every tracker adapter must do, run on
//! each of them. It asserts relative times only (order, last edit not before
//! creation), never absolute ones, so a tracker that stamps comments with its
//! own clock passes the same checks as one replayed from a recording.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Mutex};

use owlshift_adapters::tracker::linear::{ApiKey, LinearTracker};
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_adapters::tracker::{
    Author, Blocker, Capability, Column, ErrorKind, MAX_DOCUMENT_CHARS, Person, Proposal,
    StateType, Tracker,
};
use owlshift_contracts::Priority;
use owlshift_contracts::config::Admit;
use owlshift_contracts::ids::TicketId;
use serde_json::{Value, json};

use support::{Answer, Fake, Recorder, Replay};

/// What the suite knows about the tracker it checks.
struct Case {
    /// A ticket that exists, and what reading it must give.
    existing: TicketId,
    title: &'static str,
    description_start: &'static str,
    priority: Priority,
    /// Whether it has an assignee. Who it is differs between a live run and
    /// its pseudonymized recording, so the suite checks presence only.
    assigned: bool,
    /// Whether the adapter reports its assignee's account as its author, by
    /// the account's identifier: what makes the description the decider's.
    author_is_assignee: bool,
    labels: &'static [&'static str],
    /// A ticket that does not exist.
    missing: TicketId,
    /// The body of the comment the suite posts.
    body: &'static str,
}

fn id(id: &str) -> TicketId {
    TicketId::new(id).unwrap()
}

/// One shared check. A capability is covered by the checks [`check_of`]
/// names for it, and an adapter that declares the capability must have a
/// case for each of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Check {
    /// Reading a ticket and its comments, and posting a comment.
    Read,
    /// Moving and reading the visible stage.
    Stage,
    /// Listing the admitted tickets and reading who admitted them.
    Admitted,
    /// The state type, the relations and the `blocked_by` write.
    Blocked,
    /// The links an intake brief reads.
    Links,
    /// Filing a proposal and finding it again.
    Proposals,
    /// Publishing and updating a document.
    Documents,
    /// Resolving a comment.
    Folding,
    /// Reading a comment's reactions.
    Reactions,
    /// A capability with no trait method: no adapter may declare it.
    Undeclared,
}

/// The checks that cover a capability. The match is exhaustive and has no
/// wildcard, so a capability added to the trait does not compile here until
/// it is given a check.
fn check_of(capability: Capability) -> &'static [Check] {
    match capability {
        Capability::ReadTicket | Capability::Comments => &[Check::Read],
        Capability::VisibleStage => &[Check::Stage],
        Capability::ListAdmitted => &[Check::Admitted],
        // Both halves: an adapter cannot satisfy it with the relations alone.
        Capability::BlockedBy => &[Check::Blocked, Check::Links],
        Capability::ProposalsInbox => &[Check::Proposals],
        Capability::Documents => &[Check::Documents],
        Capability::FoldComments => &[Check::Folding],
        Capability::Reactions => &[Check::Reactions],
        Capability::AgentIdentity | Capability::Webhooks => &[Check::Undeclared],
    }
}

/// An adapter under test: what it declares and one case per check it runs.
/// `declared` is the adapter's own `CAPABILITIES`, so the registry cannot
/// drift from what the adapter says it does. A case builds its own world
/// and transport, calls the shared check and, where it replays a recording,
/// ends by asserting that every recorded exchange was used.
struct Adapter {
    name: &'static str,
    declared: &'static [Capability],
    cases: Vec<(Check, Box<dyn Fn()>)>,
}

impl Adapter {
    fn new(name: &'static str, declared: &'static [Capability]) -> Self {
        Self {
            name,
            declared,
            cases: Vec::new(),
        }
    }

    fn case(mut self, check: Check, case: impl Fn() + 'static) -> Self {
        self.cases.push((check, Box::new(case)));
        self
    }
}

/// Runs every case of the adapter.
fn run(adapter: &Adapter) {
    for (check, case) in &adapter.cases {
        eprintln!("{}: the {check:?} check", adapter.name);
        case();
    }
}

/// Runs the cases of one check, and fails if the adapter has none.
fn run_check(adapter: &Adapter, wanted: Check) {
    let mut ran = false;
    for (check, case) in &adapter.cases {
        if *check == wanted {
            eprintln!("{}: the {check:?} check", adapter.name);
            case();
            ran = true;
        }
    }
    assert!(ran, "{} has no {wanted:?} case", adapter.name);
}

/// The capabilities the adapter declares for which a check has no case.
fn missing_cases(adapter: &Adapter) -> Vec<Capability> {
    adapter
        .declared
        .iter()
        .copied()
        .filter(|capability| {
            check_of(*capability)
                .iter()
                .any(|check| !adapter.cases.iter().any(|(have, _)| have == check))
        })
        .collect()
}

#[test]
fn a_declared_capability_without_a_case_is_reported() {
    let none = Adapter::new("none", &[Capability::FoldComments]);
    assert_eq!(missing_cases(&none), [Capability::FoldComments]);
    // BlockedBy needs both halves.
    let half = Adapter::new("half", &[Capability::BlockedBy]).case(Check::Blocked, || {});
    assert_eq!(missing_cases(&half), [Capability::BlockedBy]);
    let whole = half.case(Check::Links, || {});
    assert!(missing_cases(&whole).is_empty());
}

/// Runs the whole suite. The calls are always made in the same order, which
/// is the order of the recorded Linear exchanges.
fn check(tracker: &dyn Tracker, case: &Case) {
    // The suite exercises reading tickets and comments: both are declared.
    let declared = tracker.capabilities();
    for needed in [Capability::ReadTicket, Capability::Comments] {
        assert!(declared.contains(&needed), "{needed:?} is not declared");
    }

    let ticket = tracker.ticket(&case.existing).unwrap();
    assert_eq!(ticket.id, case.existing);
    assert_eq!(ticket.title, case.title);
    assert!(
        ticket.description.starts_with(case.description_start),
        "{:?}",
        ticket.description
    );
    assert_eq!(ticket.priority, case.priority);
    assert_eq!(
        ticket.assignee.is_some(),
        case.assigned,
        "{:?}",
        ticket.assignee
    );
    assert_eq!(ticket.labels, case.labels);
    let author_is_assignee = matches!(
        (&ticket.author, &ticket.assignee),
        (Author::Account(author), Some(assignee)) if author.id == assignee.id
    );
    assert_eq!(
        author_is_assignee, case.author_is_assignee,
        "{:?} for {:?}",
        ticket.author, ticket.assignee
    );

    let missing = tracker.ticket(&case.missing).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");

    let before = tracker.comments(&case.existing).unwrap();
    for pair in before.windows(2) {
        assert!(pair[0].created_at <= pair[1].created_at, "not oldest first");
    }
    for comment in &before {
        assert!(comment.last_edit() >= comment.created_at);
    }

    let missing = tracker.comments(&case.missing).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");

    let missing = tracker.post_comment(&case.missing, case.body).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");

    let posted = tracker.post_comment(&case.existing, case.body).unwrap();
    assert_eq!(posted.body, case.body);
    assert!(
        matches!(posted.author, Author::Account(_)),
        "{:?}",
        posted.author
    );
    assert_eq!(posted.edited_at, None);

    let after = tracker.comments(&case.existing).unwrap();
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(after[..before.len()], before[..]);
    assert_eq!(
        after.last(),
        Some(&posted),
        "the posted comment reads back, last"
    );
}

/// The visible stage, checked apart from [`check`] so that each adapter's
/// recording of it stays separate from the read-and-comment one: the stage
/// moves to `there`, then back to `back`, and a ticket that does not exist is
/// not found. Reading the stage back is each adapter's own test.
fn check_stage(
    tracker: &dyn Tracker,
    existing: &TicketId,
    missing: &TicketId,
    there: &str,
    back: &str,
) {
    assert!(
        tracker.capabilities().contains(&Capability::VisibleStage),
        "VisibleStage is not declared"
    );
    tracker.set_stage(existing, there).unwrap();
    tracker.set_stage(existing, back).unwrap();
    let missing = tracker.set_stage(missing, there).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");
}

/// What the Admitted check knows about the tracker.
struct AdmittedCase {
    team: Option<&'static str>,
    ready: &'static str,
    admit: Admit,
    /// Tickets the listing must show, each with the blockers it must carry.
    expected: Vec<(TicketId, Vec<Blocker>)>,
    /// Tickets the listing must not show: the ones that carry the wrong
    /// label or sit in another stage. Where the tracker filters on its
    /// server a recording cannot show that filter working: the check then
    /// only shows the adapter does not invent tickets.
    absent: Vec<TicketId>,
    /// Admission gestures to read, in order.
    gestures: Vec<(TicketId, Admit)>,
    /// Who made each of them.
    by: Author,
    /// Whether the tracker keeps times: a ticket's creation in the listing
    /// and a gesture's time. A tracker that keeps none gives `None`.
    times: bool,
}

/// Listing the admitted tickets and reading who admitted them. `list` makes
/// the listing calls and `gestures` the gesture reads, which are the same
/// tracker for an adapter that is not replayed.
fn check_admitted(list: &dyn Tracker, gestures: &dyn Tracker, case: &AdmittedCase) {
    assert!(
        list.capabilities().contains(&Capability::ListAdmitted),
        "ListAdmitted is not declared"
    );
    let column = Column {
        team: case.team,
        ready: case.ready,
        admit: &case.admit,
    };
    let listed = list.admitted(&column).unwrap();
    for (n, one) in listed.iter().enumerate() {
        let id = &one.ticket.id;
        assert!(listed[..n].iter().all(|l| l.ticket.id != *id), "{id} twice");
        assert_eq!(one.created_at.is_some(), case.times, "{id}: {one:?}");
        for (m, blocker) in one.blockers.iter().enumerate() {
            let twice = one.blockers[..m].iter().any(|b| b.id == blocker.id);
            assert!(!twice, "{id}: blocker {} twice", blocker.id);
        }
    }
    for (id, blockers) in &case.expected {
        let one = listed.iter().find(|l| l.ticket.id == *id);
        let one = one.unwrap_or_else(|| panic!("{id} is not listed: {listed:?}"));
        assert_eq!(one.blockers, *blockers, "the blockers of {id}");
    }
    for id in &case.absent {
        assert!(
            listed.iter().all(|l| l.ticket.id != *id),
            "{id} is listed though it is not admitted"
        );
    }

    // The `delegation` gesture is not built: both reads refuse it.
    let refused = list
        .admitted(&Column {
            admit: &Admit::Delegation,
            ..column
        })
        .unwrap_err();
    assert_eq!(refused.kind, ErrorKind::Other, "{refused}");

    for (id, admit) in &case.gestures {
        let gesture = gestures.gesture(id, admit).unwrap();
        assert_eq!(gesture.account, case.by, "the gesture on {id}");
        assert_eq!(gesture.at.is_some(), case.times, "{id}: {gesture:?}");
    }
    let (first, _) = &case.gestures[0];
    let refused = gestures.gesture(first, &Admit::Delegation).unwrap_err();
    assert_eq!(refused.kind, ErrorKind::Other, "{refused}");
}

/// What the Blocked check knows about the tracker.
struct BlockedCase {
    /// A ticket whose state is a completed one, and one whose state is not.
    done: TicketId,
    open: TicketId,
    /// A ticket that does not exist.
    missing: TicketId,
    /// A ticket, and the blockers its relations must show.
    blocked: TicketId,
    blockers: Vec<Blocker>,
    /// Two tickets with no relation: the check writes that the first blocks
    /// the second.
    new_blocker: TicketId,
    new_blocked: TicketId,
    /// A ticket the tracker refuses to relate, when the adapter says so:
    /// the trait promises nothing about an unknown ticket, and the adapters
    /// differ, so only a case that knows its adapter refuses sets it.
    unknown_blocked: Option<TicketId>,
}

/// The state type, the relations and the `blocked_by` write: a ticket done
/// is completed and one open is not; an unknown ticket is not found; the
/// relations name the blockers; a relation written shows in them, and
/// writing it twice changes nothing.
fn check_blocked_by(tracker: &dyn Tracker, case: &BlockedCase) {
    assert!(
        tracker.capabilities().contains(&Capability::BlockedBy),
        "BlockedBy is not declared"
    );
    let done = tracker.state_type(&case.done).unwrap();
    assert_eq!(done, Some(StateType::Completed), "{}", case.done);
    let open = tracker.state_type(&case.open).unwrap();
    assert_ne!(open, Some(StateType::Completed), "{}", case.open);
    let missing = tracker.state_type(&case.missing).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");

    let relations = tracker.relations(&case.blocked).unwrap();
    assert_eq!(relations.id, case.blocked);
    assert_eq!(relations.blockers, case.blockers);
    let missing = tracker.relations(&case.missing).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");

    let before = tracker.relations(&case.new_blocked).unwrap().blockers;
    assert!(
        before.iter().all(|b| b.id != case.new_blocker),
        "{} already blocks {}",
        case.new_blocker,
        case.new_blocked
    );
    tracker.block(&case.new_blocker, &case.new_blocked).unwrap();
    let after = tracker.relations(&case.new_blocked).unwrap().blockers;
    assert_eq!(after.len(), before.len() + 1, "{after:?}");
    assert_eq!(
        after.iter().filter(|b| b.id == case.new_blocker).count(),
        1,
        "the new blocker is shown once: {after:?}"
    );
    tracker.block(&case.new_blocker, &case.new_blocked).unwrap();
    let again = tracker.relations(&case.new_blocked).unwrap().blockers;
    assert_eq!(again, after, "writing a relation twice changes nothing");

    if let Some(unknown) = &case.unknown_blocked {
        let refused = tracker.block(&case.new_blocker, unknown);
        assert!(refused.is_err(), "a relation to {unknown} was accepted");
    }
}

/// What the Links check knows about the tracker: a ticket `blocked` that is
/// blocked by `blocker`.
struct LinksCase {
    blocked: TicketId,
    blocker: TicketId,
}

/// Intake's links: the blocked ticket lists its blocker in `blocked_by` and
/// the blocker lists it in `blocks`, each once, with a title and a state.
fn check_links(tracker: &dyn Tracker, case: &LinksCase) {
    assert!(
        tracker.capabilities().contains(&Capability::BlockedBy),
        "BlockedBy is not declared"
    );
    let blocked = tracker.links(&case.blocked).unwrap();
    let blocker = tracker.links(&case.blocker).unwrap();
    for (list, wanted) in [
        (&blocked.blocked_by, &case.blocker),
        (&blocker.blocks, &case.blocked),
    ] {
        let found: Vec<_> = list.iter().filter(|l| l.id == *wanted).collect();
        assert_eq!(found.len(), 1, "{wanted} in {list:?}");
        assert!(found[0].title.as_deref().is_some_and(|t| !t.is_empty()));
        assert!(found[0].state.as_ref().is_some_and(|s| !s.name.is_empty()));
    }
    for list in [
        &blocked.blocked_by,
        &blocked.blocks,
        &blocked.related,
        &blocker.blocked_by,
        &blocker.blocks,
        &blocker.related,
    ] {
        for (n, linked) in list.iter().enumerate() {
            assert!(list[..n].iter().all(|earlier| earlier.id != linked.id));
        }
    }
}

/// What the Proposals check knows about the tracker.
struct ProposalsCase {
    team: Option<&'static str>,
    /// The ticket the proposal follows up.
    parent: TicketId,
    /// A ready state's name that is not the inbox.
    ready: &'static str,
    /// The name of the state the inbox is.
    inbox: &'static str,
}

/// Filing a proposal and finding it again: it is filed in the inbox, with
/// nothing gone wrong; it is found by its title in any case and by its
/// marker, each ticket once; another title and marker find nothing; a blank
/// title or marker is refused; and a ready state that is the inbox is
/// refused before anything is written.
fn check_proposals(tracker: &dyn Tracker, case: &ProposalsCase) {
    assert!(
        tracker.capabilities().contains(&Capability::ProposalsInbox),
        "ProposalsInbox is not declared"
    );
    const TITLE: &str = "Cap the retries";
    const MARKER: &str = "fingerprint 0123456789ab";
    let description = format!("Why.\n\n[owlshift] PROPOSAL · OWL-252 · {MARKER}");
    let proposal = Proposal {
        parent: &case.parent,
        title: TITLE,
        description: &description,
        blocked_by_parent: true,
        ready: case.ready,
    };
    let team = case.team;
    let none = tracker.proposed(team, TITLE, MARKER).unwrap();
    assert!(none.is_empty(), "found before filing: {none:?}");

    let filed = tracker.propose(&proposal).unwrap();
    assert_eq!(filed.state, case.inbox, "{filed:?}");
    assert!(filed.problems.is_empty(), "{:?}", filed.problems);

    // By its title alone, in another case; by its marker alone; by both,
    // where it is still listed once.
    for (title, marker) in [
        (TITLE.to_uppercase(), "no such marker"),
        ("Another title".to_owned(), MARKER),
        (TITLE.to_owned(), MARKER),
    ] {
        let found = tracker.proposed(team, &title, marker).unwrap();
        let ours: Vec<_> = found.iter().filter(|f| f.id == filed.id).collect();
        assert_eq!(ours.len(), 1, "{title:?} {marker:?}: {found:?}");
        assert!(ours[0].description.contains(MARKER), "{:?}", ours[0]);
    }
    let found = tracker
        .proposed(team, "Another title", "fingerprint ffffffffffff")
        .unwrap();
    assert!(found.is_empty(), "{found:?}");
    for (title, marker) in [(" ", MARKER), (TITLE, " ")] {
        let refused = tracker.proposed(team, title, marker);
        assert!(refused.is_err(), "{title:?} {marker:?}: {refused:?}");
    }

    // A proposal filed in the ready column would enter the queue without a
    // person: refused, and nothing is written.
    let refused = Proposal {
        title: "Never filed",
        description: "Why.\n\n[owlshift] PROPOSAL · OWL-252 · fingerprint aaaaaaaaaaaa",
        ready: case.inbox,
        ..proposal
    };
    let error = tracker.propose(&refused).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Other, "{error}");
    let found = tracker
        .proposed(team, refused.title, "fingerprint aaaaaaaaaaaa")
        .unwrap();
    assert!(
        found.is_empty(),
        "the refused proposal was filed: {found:?}"
    );
}

/// What the Documents check knows about the tracker.
struct DocumentsCase {
    /// A ticket that exists, and one that does not.
    ticket: TicketId,
    missing: TicketId,
}

/// Publishing a document: it is created, then its content is replaced in
/// place under the same identifier; a document the tracker does not show is
/// not found, so the runner creates another; a document of an unknown ticket
/// is an error (the trait promises no more, and Linear's answer to it was
/// never checked live); and content past the cap is refused.
fn check_documents(tracker: &dyn Tracker, case: &DocumentsCase) {
    assert!(
        tracker.capabilities().contains(&Capability::Documents),
        "Documents is not declared"
    );
    let title = format!("Plan · {}", case.ticket);
    let created = tracker
        .publish_document(&case.ticket, None, &title, "v1")
        .unwrap();
    assert!(!created.id.is_empty(), "{created:?}");
    let updated = tracker
        .publish_document(&case.ticket, Some(&created.id), &title, "v2")
        .unwrap();
    assert_eq!(updated.id, created.id, "an update is in place");

    let gone = tracker
        .publish_document(&case.ticket, Some("does-not-exist"), &title, "v2")
        .unwrap_err();
    assert_eq!(gone.kind, ErrorKind::NotFound, "{gone}");
    let unknown = tracker.publish_document(&case.missing, None, &title, "v1");
    assert!(unknown.is_err(), "{unknown:?}");

    let long = "y".repeat(MAX_DOCUMENT_CHARS + 1);
    let refused = tracker.publish_document(&case.ticket, None, &title, &long);
    assert!(refused.is_err(), "content past the cap was accepted");
}

const BODY: &str = "[owlshift] OWL-13 conformance check: a comment posted by the Linear \
                    adapter's fixture recorder · safe to delete.\n\nSecond line.";

/// A fresh Markdown tracker over a temporary tree holding `DEMO-1`, with two
/// comments. Each case builds its own, so no check depends on another.
fn markdown_world() -> (tempfile::TempDir, MarkdownTracker) {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("tickets/DEMO-1");
    fs::create_dir_all(dir.join("comments")).unwrap();
    fs::write(
        dir.join("ticket.md"),
        "+++\ntitle = \"Add a greeting\"\nauthor = \"maintainer\"\nstage = \"Todo\"\n\
         priority = \"medium\"\nassignee = \"maintainer\"\nlabels = [\"Feature\", \"Docs\"]\n+++\n\n\
         Say hello in the README.\n",
    )
    .unwrap();
    fs::write(
        dir.join("comments/20260927T090000Z-reporter.md"),
        "First.\n",
    )
    .unwrap();
    fs::write(
        dir.join("comments/20260927T100000Z-maintainer.md"),
        "Second.\n",
    )
    .unwrap();
    // The tickets the other checks read: `DEMO-2` and `DEMO-6` are the
    // admitted ones (Todo, the label), `DEMO-2` blocked by a ticket Done and
    // one still In Progress; `DEMO-1`, `DEMO-5` (Todo without the label),
    // `DEMO-3` and `DEMO-4` (labelled, in other stages) are not.
    let agent = &["agent"][..];
    for (id, title, stage, labels, blocked_by) in [
        (
            "DEMO-2",
            "Greet by name",
            "Todo",
            agent,
            &["DEMO-3", "DEMO-4"][..],
        ),
        ("DEMO-3", "Add a README", "Done", agent, &[][..]),
        ("DEMO-4", "Add a licence", "In Progress", agent, &[][..]),
        ("DEMO-5", "Add a changelog", "Todo", &[][..], &[][..]),
        ("DEMO-6", "Add a contributing guide", "Todo", agent, &[][..]),
    ] {
        add_ticket(root.path(), id, title, stage, labels, blocked_by);
    }
    let tracker = MarkdownTracker::new(root.path());
    (root, tracker)
}

/// Writes the ticket folder `id` under `root`, authored by the maintainer.
fn add_ticket(
    root: &std::path::Path,
    id: &str,
    title: &str,
    stage: &str,
    labels: &[&str],
    blocked_by: &[&str],
) {
    let dir = root.join("tickets").join(id);
    fs::create_dir_all(dir.join("comments")).unwrap();
    let list = |names: &[&str]| {
        let quoted: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
        format!("[{}]", quoted.join(", "))
    };
    let mut front = format!(
        "title = \"{title}\"\nauthor = \"maintainer\"\nstage = \"{stage}\"\nlabels = {}\n",
        list(labels)
    );
    if !blocked_by.is_empty() {
        front += &format!("blocked_by = {}\n", list(blocked_by));
    }
    fs::write(
        dir.join("ticket.md"),
        format!("+++\n{front}+++\n\n{title}.\n"),
    )
    .unwrap();
}

fn markdown_admitted_case() -> AdmittedCase {
    let maintainer = Person {
        id: "maintainer".to_owned(),
        name: "maintainer".to_owned(),
    };
    AdmittedCase {
        team: None,
        ready: "Todo",
        admit: Admit::Label("agent".to_owned()),
        expected: vec![
            (
                id("DEMO-2"),
                vec![
                    // A ticket Done is completed; one In Progress has no type.
                    Blocker {
                        id: id("DEMO-3"),
                        state: Some(StateType::Completed),
                    },
                    Blocker {
                        id: id("DEMO-4"),
                        state: None,
                    },
                ],
            ),
            (id("DEMO-6"), vec![]),
        ],
        absent: vec![id("DEMO-1"), id("DEMO-3"), id("DEMO-4"), id("DEMO-5")],
        gestures: vec![
            (id("DEMO-2"), Admit::Label("agent".to_owned())),
            (id("DEMO-6"), Admit::State("Todo".to_owned())),
        ],
        by: Author::Account(maintainer),
        times: false,
    }
}

fn markdown_case() -> Case {
    Case {
        existing: id("DEMO-1"),
        title: "Add a greeting",
        description_start: "Say hello",
        priority: Priority::Medium,
        assigned: true,
        author_is_assignee: true,
        labels: &["Feature", "Docs"],
        missing: id("DEMO-404"),
        body: BODY,
    }
}

/// The Markdown tracker, on a tree of files built for each case.
fn markdown() -> Adapter {
    Adapter::new("the Markdown tracker", MarkdownTracker::CAPABILITIES)
        .case(Check::Read, || {
            let (_world, tracker) = markdown_world();
            let case = markdown_case();
            check(&tracker, &case);
            let maintainer = Person {
                id: "maintainer".to_owned(),
                name: "maintainer".to_owned(),
            };
            assert_eq!(
                Tracker::ticket(&tracker, &case.existing).unwrap().assignee,
                Some(maintainer)
            );
        })
        .case(Check::Stage, || {
            let (_world, tracker) = markdown_world();
            let case = markdown_case();
            check_stage(
                &tracker,
                &case.existing,
                &case.missing,
                "Needs Input",
                "In Progress",
            );
            assert_eq!(tracker.ticket(&case.existing).unwrap().stage, "In Progress");
        })
        .case(Check::Admitted, || {
            let (_world, tracker) = markdown_world();
            let case = markdown_admitted_case();
            // The tickets that must not be listed are there to be left out.
            for id in &case.absent {
                Tracker::ticket(&tracker, id).unwrap();
            }
            check_admitted(&tracker, &tracker, &case);
        })
        .case(Check::Blocked, || {
            let (_world, tracker) = markdown_world();
            check_blocked_by(&tracker, &markdown_blocked_case());
        })
        .case(Check::Links, || {
            // `DEMO-2`'s `blocked_by` names `DEMO-3`: the file of `DEMO-3`
            // says nothing, so its `blocks` comes from the scan of the tree.
            let (_world, tracker) = markdown_world();
            check_links(
                &tracker,
                &LinksCase {
                    blocked: id("DEMO-2"),
                    blocker: id("DEMO-3"),
                },
            );
        })
        .case(Check::Proposals, || {
            let (world, tracker) = markdown_world();
            let tickets = || fs::read_dir(world.path().join("tickets")).unwrap().count();
            let before = tickets();
            check_proposals(
                &tracker,
                &ProposalsCase {
                    team: None,
                    parent: id("DEMO-1"),
                    ready: "Todo",
                    inbox: MarkdownTracker::TRIAGE,
                },
            );
            // The proposal is a ticket of its own, and the refused one none.
            assert_eq!(tickets(), before + 1);
        })
        .case(Check::Documents, || {
            let (world, tracker) = markdown_world();
            check_documents(
                &tracker,
                &DocumentsCase {
                    ticket: id("DEMO-1"),
                    missing: id("DEMO-404"),
                },
            );
            // One file, its content replaced in place under its title; the
            // refused calls wrote none.
            let dir = world.path().join("tickets/DEMO-1/documents");
            let files: Vec<_> = fs::read_dir(&dir).unwrap().collect();
            assert_eq!(files.len(), 1, "{files:?}");
            let text = fs::read_to_string(dir.join("1.md")).unwrap();
            assert!(text.starts_with("# Plan · DEMO-1"), "{text:?}");
            assert!(text.contains("v2") && !text.contains("v1"), "{text:?}");
        })
}

fn markdown_blocked_case() -> BlockedCase {
    BlockedCase {
        done: id("DEMO-3"),
        open: id("DEMO-4"),
        missing: id("DEMO-404"),
        blocked: id("DEMO-2"),
        blockers: vec![
            Blocker {
                id: id("DEMO-3"),
                state: Some(StateType::Completed),
            },
            Blocker {
                id: id("DEMO-4"),
                state: None,
            },
        ],
        new_blocker: id("DEMO-6"),
        new_blocked: id("DEMO-5"),
        // The Markdown tracker refuses a relation on a ticket it lacks.
        unknown_blocked: Some(id("DEMO-404")),
    }
}

#[test]
fn the_markdown_tracker_conforms() {
    run(&markdown());
}

fn linear_case() -> Case {
    Case {
        existing: id("OWL-13"),
        title: "Add the Linear tracker adapter: read a ticket and post a comment",
        description_start: "**Why.**",
        priority: Priority::Medium,
        assigned: true,
        // Its assignee's account created it, but a holder of that account's
        // API key could have (OWL-74): Linear never reports the account.
        author_is_assignee: false,
        labels: &["Feature", "Adapters"],
        missing: id("OWL-99999"),
        body: BODY,
    }
}

const LINEAR_FIXTURE: &str = "conformance.json";

/// The ticket the stage fixture moves, from In Progress to Needs Input and
/// back: the issue of the change that recorded it.
const STAGE_TICKET: &str = "OWL-137";
const STAGE_FIXTURE: &str = "stage-owl-137.json";

/// Linear, on its recorded exchanges.
fn linear() -> Adapter {
    Adapter::new("Linear", LinearTracker::CAPABILITIES)
        .case(Check::Read, || {
            let replay = Replay::new(LINEAR_FIXTURE);
            check(
                &LinearTracker::with_transport(replay.clone()),
                &linear_case(),
            );
            replay.assert_done();
        })
        .case(Check::Stage, || {
            let replay = Replay::new(STAGE_FIXTURE);
            check_stage(
                &LinearTracker::with_transport(replay.clone()),
                &id(STAGE_TICKET),
                &linear_case().missing,
                "Needs Input",
                "In Progress",
            );
            replay.assert_done();
        })
        .case(Check::Admitted, || {
            // Team `OWL`'s Todo on 2026-10-13 (a state gesture), then the
            // four gestures recorded for the drain, each from its fixture.
            let listing = Replay::new("admitted-todo.json");
            let gestures = Replay::new("gestures.json");
            check_admitted(
                &LinearTracker::with_transport(listing.clone()),
                &LinearTracker::with_transport(gestures.clone()),
                &linear_admitted_case(),
            );
            listing.assert_done();
            gestures.assert_done();
        })
        .case(Check::Blocked, || {
            // MADE UP: no recording holds these answers. `Fake` answers the
            // state type, the relations and the relation write in the
            // shapes of the unit tests of `src/tracker/linear.rs`, and keeps
            // the relations written so that the next read shows them. To be
            // recorded live by a later ticket.
            check_blocked_by(
                &LinearTracker::with_transport(linear_relations()),
                &linear_blocked_case(),
            );
        })
        .case(Check::Links, || {
            // Recorded on 2026-10-21: OWL-254 is blocked by OWL-267, which
            // blocks it.
            let replay = Replay::new(LINKS_FIXTURE);
            check_links(
                &LinearTracker::with_transport(replay.clone()),
                &LinksCase {
                    blocked: id("OWL-254"),
                    blocker: id("OWL-267"),
                },
            );
            replay.assert_done();
        })
        .case(Check::Proposals, || {
            // MADE UP: no recording holds these answers (see `Fake`).
            let (fake, stored) = linear_proposals();
            check_proposals(
                &LinearTracker::with_transport(fake),
                &ProposalsCase {
                    team: Some("owl"),
                    parent: id("OWL-252"),
                    ready: "Todo",
                    inbox: "Triage",
                },
            );
            // One issue was created: the refused proposal never was.
            assert_eq!(stored.lock().unwrap().len(), 1);
        })
        .case(Check::Documents, || {
            // MADE UP: no recording holds these answers (see `Fake`). The
            // answer to a document of an unknown issue is not even in a
            // unit test: it is unverified, and the check asks only for an
            // error.
            let fake = linear_documents();
            check_documents(
                &LinearTracker::with_transport(fake.clone()),
                &DocumentsCase {
                    ticket: id("OWL-1"),
                    missing: id("OWL-99999"),
                },
            );
            // The refused content was never sent.
            assert_eq!(
                fake.asked(),
                [
                    "CreateDocument",
                    "UpdateDocument",
                    "UpdateDocument",
                    "CreateDocument"
                ]
            );
        })
}

/// MADE UP: a Linear that knows the issue OWL-1 and keeps the documents
/// made on it, in the answer shapes of the unit tests of
/// `src/tracker/linear.rs`. An update of a document it does not hold is
/// `Entity not found: Document`; a document of another issue is
/// `Entity not found: Issue`, UNVERIFIED: nothing recorded or tested says
/// how Linear answers it.
fn linear_documents() -> Fake {
    let mut documents: BTreeMap<String, String> = BTreeMap::new();
    let written = |id: &str| {
        json!({ "success": true, "document": { "id": id,
            "url": format!("https://linear.app/owlshift/document/{id}"), "archivedAt": null } })
    };
    Fake::new(move |name, variables| match name {
        "CreateDocument" => {
            let input = &variables["input"];
            if input["issueId"] != "OWL-1" {
                return Answer::NotFound("Entity not found: Issue");
            }
            let id = format!("d{}", documents.len() + 1);
            documents.insert(id.clone(), input["content"].as_str().unwrap().to_owned());
            Answer::Data(json!({ "documentCreate": written(&id) }))
        }
        "UpdateDocument" => {
            let id = variables["id"].as_str().unwrap();
            let Some(content) = documents.get_mut(id) else {
                return Answer::NotFound("Entity not found: Document");
            };
            *content = variables["input"]["content"].as_str().unwrap().to_owned();
            Answer::Data(json!({ "documentUpdate": written(id) }))
        }
        other => panic!("the made-up Linear does not answer {other}"),
    })
}

const LINKS_FIXTURE: &str = "links.json";

/// The issues the made-up Linear stores: identifier, title, description.
type Stored = Vec<(String, String, String)>;

/// MADE UP: a Linear whose team `OWL` has triage on, in the answer shapes of
/// the unit tests of `src/tracker/linear.rs`. It stores each issue
/// `Propose` creates, which `Proposed` finds again by the title, whatever
/// its case, and by the description's marker, as the two filters do.
fn linear_proposals() -> (Fake, Arc<Mutex<Stored>>) {
    let stored: Arc<Mutex<Stored>> = Arc::default();
    let kept = Arc::clone(&stored);
    let states = [
        ("In Review", "started", 2.6),
        ("Triage", "triage", 0.0),
        ("Backlog", "backlog", 0.0),
        ("Todo", "unstarted", 1.0),
        ("Done", "completed", 3.0),
        ("Canceled", "canceled", 4.0),
    ];
    let fake = Fake::new(move |name, variables| match name {
        "ProposalTarget" => {
            let nodes: Vec<Value> = states
                .iter()
                .map(|(name, kind, position)| {
                    json!({ "id": format!("s-{name}"), "name": name, "type": kind,
                        "position": position })
                })
                .collect();
            Answer::Data(
                json!({ "issue": { "id": "parent-id", "project": { "id": "p4" },
                "team": { "id": "team-owl", "key": "OWL", "triageEnabled": true,
                    "states": { "nodes": nodes, "pageInfo": { "hasNextPage": false } } } } }),
            )
        }
        "Propose" => {
            let input = &variables["input"];
            let mut stored = kept.lock().unwrap();
            let identifier = format!("OWL-{}", 266 + stored.len());
            stored.push((
                identifier.clone(),
                input["title"].as_str().unwrap().to_owned(),
                input["description"].as_str().unwrap().to_owned(),
            ));
            let state = input["stateId"].as_str().unwrap();
            Answer::Data(json!({ "issueCreate": { "success": true, "issue": {
                "id": "new-id", "identifier": identifier,
                "state": { "id": state, "name": state.trim_start_matches("s-") } } } }))
        }
        "Relate" => Answer::Data(json!({ "issueRelationCreate": { "success": true } })),
        "Proposed" => {
            let filter = &variables["filter"];
            let by_title = filter["title"]["eqIgnoreCase"].as_str();
            let by_marker = filter["description"]["contains"].as_str();
            let nodes: Vec<Value> = kept
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, title, description)| {
                    by_title.is_some_and(|t| t.to_lowercase() == title.to_lowercase())
                        || by_marker.is_some_and(|m| description.contains(m))
                })
                .map(|(identifier, _, description)| {
                    json!({ "identifier": identifier, "description": description,
                        "state": { "type": "triage" } })
                })
                .collect();
            Answer::Data(json!({ "issues": { "nodes": nodes,
                "pageInfo": { "hasNextPage": false, "endCursor": null } } }))
        }
        other => panic!("the made-up Linear does not answer {other}"),
    });
    (fake, stored)
}

fn linear_blocked_case() -> BlockedCase {
    BlockedCase {
        done: id("OWL-1"),
        open: id("OWL-2"),
        missing: id("OWL-99999"),
        blocked: id("OWL-7"),
        blockers: vec![Blocker {
            id: id("OWL-2"),
            state: Some(StateType::Started),
        }],
        new_blocker: id("OWL-3"),
        new_blocked: id("OWL-4"),
        // Linear's answer to a relation on an unknown blocked ticket was
        // never checked live: only the unknown blocker was (check (d)).
        unknown_blocked: None,
    }
}

/// MADE UP: a Linear that knows the tickets OWL-1 (completed), OWL-2
/// (started), OWL-3 and OWL-4 (unstarted) and OWL-7, blocked by OWL-2, and
/// answers every other issue `Entity not found: Issue`. A `blocks` relation
/// written is kept once and read back by `Relations`, as Linear answers a
/// relation already there (OWL-292).
fn linear_relations() -> Fake {
    let mut blockers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    blockers.insert("OWL-7".to_owned(), vec!["OWL-2".to_owned()]);
    let kind = |id: &str| match id {
        "OWL-1" => Some("completed"),
        "OWL-2" => Some("started"),
        "OWL-3" | "OWL-4" | "OWL-7" => Some("unstarted"),
        _ => None,
    };
    Fake::new(move |name, variables| match name {
        "TicketStateType" => match kind(variables["id"].as_str().unwrap()) {
            Some(kind) => Answer::Data(json!({ "issue": { "state": { "type": kind } } })),
            None => Answer::NotFound("Entity not found: Issue"),
        },
        "Relations" => {
            let id = variables["id"].as_str().unwrap();
            let Some(own) = kind(id) else {
                return Answer::NotFound("Entity not found: Issue");
            };
            let nodes: Vec<Value> = blockers
                .get(id)
                .into_iter()
                .flatten()
                .map(|blocker| {
                    json!({ "type": "blocks", "issue": {
                        "identifier": blocker, "state": { "type": kind(blocker) } } })
                })
                .collect();
            Answer::Data(json!({ "issue": {
                "identifier": id, "description": null, "state": { "type": own },
                "inverseRelations": { "nodes": nodes, "pageInfo": { "hasNextPage": false } } } }))
        }
        "Relate" => {
            let input = &variables["input"];
            assert_eq!(input["type"], "blocks", "{input}");
            let list = blockers
                .entry(input["relatedIssueId"].as_str().unwrap().to_owned())
                .or_default();
            let blocker = input["issueId"].as_str().unwrap().to_owned();
            if !list.contains(&blocker) {
                list.push(blocker);
            }
            Answer::Data(json!({ "issueRelationCreate": { "success": true } }))
        }
        other => panic!("the made-up Linear does not answer {other}"),
    })
}

/// A `NotFound` answer of the fake reads as [`ErrorKind::NotFound`] through
/// the adapter, as Linear's own does.
#[test]
fn a_not_found_answer_of_the_fake_is_not_found() {
    let tracker = LinearTracker::with_transport(linear_relations());
    let missing = tracker.state_type(&id("OWL-99999")).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");
}

fn linear_admitted_case() -> AdmittedCase {
    let blocker = |id: &str| Blocker {
        id: TicketId::new(id).unwrap(),
        state: Some(StateType::Started),
    };
    let label = |name: &str| Admit::Label(name.to_owned());
    let by = Person {
        id: "00000000-0000-4000-8000-000000000001".to_owned(),
        name: "person-1".to_owned(),
    };
    AdmittedCase {
        team: Some("owl"),
        ready: "Todo",
        admit: Admit::State("Todo".to_owned()),
        expected: vec![(id("OWL-155"), vec![blocker("OWL-246"), blocker("OWL-245")])],
        // Linear filters on its server, so a recording only shows that the
        // adapter lists no ticket the answer does not hold.
        absent: vec![id("OWL-99999")],
        // A label added by an account (OWL-129), a label set at creation and
        // a move by an account (OWL-179), a ticket created in the state
        // (OWL-249): the maintainer's account each time.
        gestures: vec![
            (id("OWL-129"), label("zone:backend/billing")),
            (id("OWL-179"), label("agent")),
            (id("OWL-179"), Admit::State("Canceled".to_owned())),
            (id("OWL-249"), Admit::State("Todo".to_owned())),
        ],
        by: Author::Account(by),
        times: true,
    }
}

#[test]
fn the_linear_tracker_conforms() {
    run(&linear());
}

/// Records the Linear conformance fixture by running the suite against the
/// live API. The suite posts a comment on OWL-13: it is sent only when
/// `OWLSHIFT_RECORD_WRITES=OWL-13`; otherwise it is made up and flagged.
/// What was recorded is written even when the suite fails, so a failed run
/// never needs a second write to be understood.
#[test]
#[ignore = "records against the live Linear API"]
fn record_conformance_fixture() {
    let key = support::live_key();
    support::assert_owlshift_workspace(&key);
    let case = linear_case();
    let writes =
        std::env::var("OWLSHIFT_RECORD_WRITES").ok().as_deref() == Some(case.existing.as_str());
    let recorder = if writes {
        Recorder::new(&key)
    } else {
        Recorder::new(&key).synthesizing_posts_on(&key, case.existing.as_str())
    };
    let tracker = LinearTracker::with_transport(recorder.clone());
    // After a panic only the recorder's log is read, never the tracker.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&tracker, &case)));
    let how = if writes {
        "recorded live"
    } else {
        "recorded live, except the comment posted on OWL-13, made up (see `synthesized`)"
    };
    recorder.write(
        LINEAR_FIXTURE,
        &format!("{} on the Owlshift workspace, {how}", today()),
    );
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn the_linear_tracker_moves_the_visible_stage() {
    run_check(&linear(), Check::Stage);
}

/// Records the stage fixture against the live API. It writes to Linear:
/// only with `OWLSHIFT_RECORD_WRITES=OWL-137`, on the Owlshift workspace,
/// and only while OWL-137 is In Progress, where the check leaves it. Once
/// the issue was found, it is put back in the state it was found in
/// whatever happens, and the state Linear answers is printed.
#[test]
#[ignore = "records against the live Linear API, and moves OWL-137"]
fn record_stage_fixture() {
    let key = support::live_key();
    support::assert_owlshift_workspace(&key);
    assert_eq!(
        std::env::var("OWLSHIFT_RECORD_WRITES").ok().as_deref(),
        Some(STAGE_TICKET),
        "refusing to move {STAGE_TICKET} without OWLSHIFT_RECORD_WRITES={STAGE_TICKET}"
    );
    let found = support::live_query(
        &key,
        &format!("{{ issue(id: \"{STAGE_TICKET}\") {{ id state {{ id name }} }} }}"),
    );
    let issue = &found["data"]["issue"];
    assert_eq!(
        issue["state"]["name"], "In Progress",
        "{STAGE_TICKET} must be In Progress to record the stage fixture"
    );
    let _restore = Restore {
        key: key.clone(),
        issue: issue["id"].as_str().unwrap().to_owned(),
        state: issue["state"]["id"].as_str().unwrap().to_owned(),
    };

    let recorder = Recorder::new(&key);
    let tracker = LinearTracker::with_transport(recorder.clone());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_stage(
            &tracker,
            &id(STAGE_TICKET),
            &linear_case().missing,
            "Needs Input",
            "In Progress",
        )
    }));
    recorder.write(
        STAGE_FIXTURE,
        &format!("{} on the Owlshift workspace, recorded live", today()),
    );
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// The ticket whose stage is read back: OWL-112, which added the read and is
/// Done, so the answer the fixture holds is the one Linear still gives.
const READ_STAGE_TICKET: &str = "OWL-112";
const READ_STAGE_FIXTURE: &str = "read-stage-owl-112.json";

fn check_read_stage(tracker: &dyn Tracker) {
    assert_eq!(tracker.stage(&id(READ_STAGE_TICKET)).unwrap(), "Done");
    let missing = tracker.stage(&linear_case().missing).unwrap_err();
    assert_eq!(missing.kind, ErrorKind::NotFound, "{missing}");
}

#[test]
fn the_linear_tracker_reads_a_tickets_stage() {
    let replay = Replay::new(READ_STAGE_FIXTURE);
    check_read_stage(&LinearTracker::with_transport(replay.clone()));
    replay.assert_done();
}

/// Records the stage read (OWL-112) against the live API. It only reads, and
/// the answer is checked against the recording's, so a ticket that has moved
/// since fails here rather than recording another stage.
#[test]
#[ignore = "records against the live Linear API"]
fn record_read_stage_fixture() {
    let key = support::live_key();
    support::assert_owlshift_workspace(&key);
    let recorder = Recorder::new(&key);
    let tracker = LinearTracker::with_transport(recorder.clone());
    let outcome =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check_read_stage(&tracker)));
    recorder.write(
        READ_STAGE_FIXTURE,
        &format!("{} on the Owlshift workspace, recorded live", today()),
    );
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Puts the issue back in the state it was found in when dropped, and says
/// whether Linear confirms it.
struct Restore {
    key: ApiKey,
    issue: String,
    state: String,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let (issue, state) = (&self.issue, &self.state);
        let moved = support::live_query(
            &self.key,
            &format!(
                "mutation {{ issueUpdate(id: \"{issue}\", input: {{ stateId: \"{state}\" }}) \
                 {{ success issue {{ state {{ id name }} }} }} }}"
            ),
        );
        let now = &moved["data"]["issueUpdate"]["issue"]["state"];
        if now["id"] == state.as_str() {
            eprintln!("{STAGE_TICKET} is back in {}", now["name"]);
        } else {
            eprintln!("RESTORE FAILED: {STAGE_TICKET} is not back in its state: {moved}");
        }
    }
}

fn today() -> String {
    jiff::Zoned::now().date().to_string()
}

/// The GitHub Issues tracker (OWL-535), on recorded exchanges of the private
/// repository `pitchopp/owlshift-live-sandbox`, whose issue #11 was opened
/// and commented on by live check G1 and G2 (build plan, "P9 ·
/// Open-source readiness"). Its reads were recorded live; its writes, the
/// comment and the labels, are made up in the shape GitHub answers them and
/// flagged `synthesized`, the reads after them amended to show them. To
/// record again, with a token that reads the repository in
/// `OWLSHIFT_GITHUB_TOKEN` or a logged-in `gh`:
///
/// ```sh
/// cargo test -p owlshift-adapters --test tracker_conformance -- --ignored record_github
/// ```
///
/// With `OWLSHIFT_RECORD_WRITES=11` the writes are sent, through the token,
/// on issue #11, and nothing is made up.
mod github {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};

    use owlshift_adapters::forge::Repo;
    use owlshift_adapters::forge::github::{GitHubForge, HttpTransport};
    use owlshift_adapters::tracker::github::GitHubTracker;
    use owlshift_adapters::tracker::{ErrorKind, Tracker};
    use owlshift_contracts::Priority;
    use owlshift_contracts::config;

    use super::support::{self, Recorder, Replay};
    use super::{Adapter, BODY, Case, Check, check, check_stage, id, run, run_check, today};

    const SANDBOX: &str = "pitchopp/owlshift-live-sandbox";
    /// Issue #11, G1's: three comments, two of them in one second (G2).
    const ISSUE: &str = "11";
    /// Pull request #18, G5's.
    const PULL_REQUEST: &str = "18";
    const CONFORMANCE: &str = "conformance.json";
    const STAGE: &str = "stage.json";
    const THERE: &str = "agent:needs-input";
    const BACK: &str = "agent:in-progress";

    fn dir() -> PathBuf {
        support::fixtures("github-tracker")
    }

    fn states() -> config::Tracker {
        config::Tracker {
            kind: config::TrackerKind::Github,
            team: None,
            admit: config::Admit::Label("agent:ready".to_owned()),
            admitters: None,
            exclude: None,
            states: config::States {
                ready: "agent:ready".to_owned(),
                working: BACK.to_owned(),
                needs_input: THERE.to_owned(),
                review: "agent:review".to_owned(),
                parked: None,
            },
            priorities: None,
        }
    }

    fn tracker(
        transport: impl owlshift_adapters::graphql::Transport + Send + Sync + 'static,
    ) -> GitHubTracker {
        let forge = GitHubForge::with_transport(transport, Repo::parse(SANDBOX).unwrap());
        GitHubTracker::new(forge, &states())
    }

    fn case() -> Case {
        Case {
            existing: id(ISSUE),
            title: "Owlshift live check: G1",
            description_start: "Probe of OWL-532",
            priority: Priority::Unset,
            assigned: false,
            // Never an account on GitHub, as on Linear (OWL-74).
            author_is_assignee: false,
            labels: &[],
            missing: id("9999"),
            body: BODY,
        }
    }

    /// What the stage fixture checks beyond [`check_stage`]: issue #11 is
    /// closed, so it reads `closed` whatever its labels, and a pull
    /// request's number names no ticket (G5).
    fn check_reads(tracker: &dyn Tracker) {
        assert_eq!(tracker.stage(&id(ISSUE)).unwrap(), "closed");
        let pull = tracker.ticket(&id(PULL_REQUEST)).unwrap_err();
        assert_eq!(pull.kind, ErrorKind::NotFound, "{pull}");
    }

    /// GitHub Issues, on its recorded exchanges. It declares ReadTicket,
    /// Comments and VisibleStage today: the cases of the capabilities that
    /// arrive with the drain's ticket (OWL-536) are added with them, and
    /// `every_declared_capability_has_a_case` fails until they are.
    pub fn adapter() -> Adapter {
        Adapter::new("GitHub Issues", GitHubTracker::CAPABILITIES)
            .case(Check::Read, || {
                let replay = Replay::of(&dir(), CONFORMANCE);
                check(&tracker(replay.clone()), &case());
                replay.assert_done();
            })
            .case(Check::Stage, || {
                let replay = Replay::of(&dir(), STAGE);
                let tracker = tracker(replay.clone());
                let case = case();
                check_stage(&tracker, &case.existing, &case.missing, THERE, BACK);
                check_reads(&tracker);
                replay.assert_done();
            })
    }

    #[test]
    fn the_github_tracker_conforms() {
        run(&adapter());
    }

    #[test]
    fn the_github_tracker_moves_the_visible_stage() {
        run_check(&adapter(), Check::Stage);
    }

    /// No GitHub account of a recording is a real one: each user object is
    /// a pseudonym, `person-n` with the id n.
    #[test]
    fn the_github_tracker_fixtures_name_no_real_account() {
        fn users(value: &Value, found: &mut Vec<Value>) {
            match value {
                Value::Object(map) if map.contains_key("login") => found.push(value.clone()),
                Value::Object(map) => map.values().for_each(|v| users(v, found)),
                Value::Array(items) => items.iter().for_each(|v| users(v, found)),
                _ => {}
            }
        }
        for name in [CONFORMANCE, STAGE, DRAIN] {
            let fixture = support::load(&dir(), name);
            let mut found = Vec::new();
            for exchange in &fixture.exchanges {
                users(&exchange.response, &mut found);
            }
            assert!(!found.is_empty(), "{name}: no user at all");
            for user in found {
                let n = user["id"].as_u64().expect("a numeric id");
                assert_eq!(user["login"], format!("person-{n}"), "{name}: {user}");
                assert_eq!(user.as_object().unwrap().len(), 3, "{name}: {user}");
            }
        }
    }

    /// What the recording made up on issue #11: the comments posted and the
    /// labels it carries since.
    #[derive(Default)]
    struct Made {
        comments: Vec<Value>,
        labels: Vec<String>,
    }

    /// The recorder over the live API. Without `OWLSHIFT_RECORD_WRITES=11`
    /// every write is made up and the reads of issue #11 amended to show
    /// it; any other write is refused.
    fn recorder() -> (Recorder, &'static str) {
        let live = Recorder::over(HttpTransport::new(support::github_token()), dir())
            .pseudonymizing_github_users();
        if std::env::var("OWLSHIFT_RECORD_WRITES").ok().as_deref() == Some(ISSUE) {
            return (live, "recorded live");
        }
        let viewer = live.live_get("user");
        let issue = format!("repos/{SANDBOX}/issues/{ISSUE}");
        let made = Arc::new(Mutex::new(Made::default()));
        let (written, path) = (Arc::clone(&made), issue.clone());
        let recorder = live
            .making_up(move |request| {
                let method = request["method"].as_str()?;
                let at = request["path"].as_str()?;
                let mut made = written.lock().unwrap();
                match (method, at.strip_prefix(&path)) {
                    ("POST", Some("/comments")) => {
                        let now = jiff::Timestamp::now()
                            .round(jiff::Unit::Second)
                            .unwrap()
                            .to_string();
                        let comment = json!({
                            "id": 9_000_000_001_u64 + made.comments.len() as u64,
                            "issue_url": format!("https://api.github.com/{path}"),
                            "user": viewer.clone(),
                            "created_at": now,
                            "updated_at": now,
                            "author_association": "OWNER",
                            "body": request["body"]["body"],
                            "performed_via_github_app": { "slug": "owl-shift" },
                        });
                        made.comments.push(comment.clone());
                        Some((
                            comment,
                            "not posted: shaped like the recorded comments".to_owned(),
                        ))
                    }
                    ("POST", Some("/labels")) => {
                        for label in request["body"]["labels"].as_array()? {
                            made.labels.push(label.as_str()?.to_owned());
                        }
                        Some((
                            labels(&made.labels),
                            "not sent: GitHub answers the issue's labels".to_owned(),
                        ))
                    }
                    ("DELETE", Some(rest)) => {
                        let gone = rest.strip_prefix("/labels/")?.replace("%3A", ":");
                        made.labels.retain(|label| *label != gone);
                        Some((
                            labels(&made.labels),
                            "not sent: GitHub answers the issue's labels".to_owned(),
                        ))
                    }
                    _ => panic!(
                        "refusing to write {method} {at} without OWLSHIFT_RECORD_WRITES={ISSUE}"
                    ),
                }
            })
            .amending(move |at, response| {
                let made = made.lock().unwrap();
                if at == issue && !made.labels.is_empty() {
                    response["labels"] = labels(&made.labels);
                    return Some("recorded, with the labels made up since".to_owned());
                }
                if at.starts_with(&format!("{issue}/comments?")) && !made.comments.is_empty() {
                    let page = response.as_array_mut()?;
                    page.extend(made.comments.iter().cloned());
                    return Some("recorded, plus the made-up comment".to_owned());
                }
                None
            });
        (
            recorder,
            "recorded live, except the writes on issue #11, made up (see `synthesized`)",
        )
    }

    /// Labels as GitHub answers them, made up.
    fn labels(names: &[String]) -> Value {
        json!(
            names
                .iter()
                .map(|name| json!({ "name": name, "color": "ededed", "default": false }))
                .collect::<Vec<_>>()
        )
    }

    #[test]
    #[ignore = "records against the live GitHub API"]
    fn record_github_conformance_fixture() {
        let (recorder, how) = recorder();
        let tracker = tracker(recorder.clone());
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&tracker, &case())));
        recorder.write(CONFORMANCE, &format!("{} on {SANDBOX}, {how}", today()));
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    /// The label of OWL-547's gesture checks (G3): #30 got it through the
    /// app's token, #31 through the maintainer's own `gh`, #32 added after
    /// the opening, through the token.
    const GESTURE_LABEL: &str = "owlshift-547-ready";
    const DRAIN: &str = "drain.json";
    const PROPOSAL_TITLE: &str = "Owlshift live check: OWL-536 proposal";
    const PROPOSAL_MARKER: &str = "fingerprint 0536abcdef01";

    /// What the drain reads (OWL-536), on the sandbox's probes of G1, G3 and
    /// G4, then the writes it makes, made up when recorded.
    fn check_drain(tracker: &dyn Tracker) {
        use owlshift_adapters::tracker::{Author, Column, Proposal, StateType};
        use owlshift_contracts::config::Admit;

        let admit = Admit::Label(GESTURE_LABEL.to_owned());
        let listed = tracker
            .admitted(&Column {
                team: None,
                ready: GESTURE_LABEL,
                admit: &admit,
            })
            .unwrap();
        let ids: Vec<&str> = listed.iter().map(|l| l.ticket.id.as_str()).collect();
        assert_eq!(ids, ["31"]);
        assert!(listed[0].blockers.is_empty());

        // G3: a label put through a person's own login is their gesture; one
        // put through the app's token is no account's; one removed since
        // stands nowhere.
        let gesture = tracker.gesture(&id("31"), &admit).unwrap();
        assert!(matches!(gesture.account, Author::Account(_)), "{gesture:?}");
        assert_eq!(gesture.at.unwrap().to_string(), "2026-11-12T19:44:38Z");
        for through_app in ["30", "32"] {
            let gesture = tracker.gesture(&id(through_app), &admit).unwrap();
            let Author::Other { name } = gesture.account else {
                panic!("{through_app}: {gesture:?}");
            };
            assert!(name.ends_with("through the GitHub App owl-shift"), "{name}");
        }
        let removed = Admit::Label("owlshift-check:ready".to_owned());
        assert!(tracker.gesture(&id("12"), &removed).is_err());

        // G4 and G5: the closing reasons, and a pull request's number.
        for (number, state) in [
            ("14", Some(StateType::Completed)),
            ("15", Some(StateType::Canceled)),
            ("16", Some(StateType::Duplicate)),
            ("29", None),
        ] {
            assert_eq!(tracker.state_type(&id(number)).unwrap(), state, "#{number}");
        }
        let pull = tracker.state_type(&id(PULL_REQUEST)).unwrap_err();
        assert_eq!(pull.kind, ErrorKind::NotFound);
        let relations = tracker.relations(&id("13")).unwrap();
        let blockers: Vec<(&str, Option<StateType>)> = relations
            .blockers
            .iter()
            .map(|b| (b.id.as_str(), b.state))
            .collect();
        assert_eq!(
            blockers,
            [
                ("15", Some(StateType::Canceled)),
                ("16", Some(StateType::Duplicate))
            ]
        );
        let links = tracker.links(&id("13")).unwrap();
        assert_eq!(links.blocked_by.len(), 2);

        // G1: a comment's reactions, its thumbs-up a person's.
        let comments = tracker.comments(&id(ISSUE)).unwrap();
        let reacted: Vec<_> = comments
            .iter()
            .flat_map(|comment| &comment.reactions)
            .collect();
        assert!(
            reacted
                .iter()
                .any(|r| r.is_thumbs_up() && matches!(r.author, Author::Account(_))),
            "{reacted:?}"
        );
        assert!(tracker.working(None, BACK).unwrap().is_empty());

        // The proposals: none yet, then one filed, blocked by its parent; a
        // relation written once.
        assert!(
            tracker
                .proposed(None, PROPOSAL_TITLE, PROPOSAL_MARKER)
                .unwrap()
                .is_empty()
        );
        let parent = id("29");
        let filed = tracker
            .propose(&Proposal {
                parent: &parent,
                title: PROPOSAL_TITLE,
                description: &format!("Why.\n\n[owlshift] PROPOSAL · 29 · {PROPOSAL_MARKER}"),
                blocked_by_parent: true,
                ready: "agent:ready",
            })
            .unwrap();
        assert_eq!(filed.state, "agent:proposed");
        assert!(filed.problems.is_empty(), "{:?}", filed.problems);
        tracker.block(&id("14"), &parent).unwrap();
    }

    #[test]
    fn the_github_tracker_drains() {
        let replay = Replay::of(&dir(), DRAIN);
        check_drain(&tracker(replay.clone()));
        replay.assert_done();
    }

    /// The recorder of the drain's reads, which only reads the sandbox:
    /// the list of the gesture's label shows #31 open with its label, as
    /// when its gesture was made (it is closed since, its labels removed),
    /// and the writes, an issue and two relations, are made up.
    fn drain_recorder() -> Recorder {
        let live = Recorder::over(HttpTransport::new(support::github_token()), dir())
            .pseudonymizing_github_users();
        let repo = format!("repos/{SANDBOX}");
        let mut opened = live.live_get(&format!("{repo}/issues/31"));
        opened["state"] = json!("open");
        opened["state_reason"] = Value::Null;
        opened["closed_at"] = Value::Null;
        opened["labels"] = labels(&[GESTURE_LABEL.to_owned()]);
        let shape = live.live_get(&format!("{repo}/issues/29"));
        let list = format!("{repo}/issues?state=open&labels={GESTURE_LABEL}&");
        let issues = format!("{repo}/issues");
        live.making_up(move |request| {
            let method = request["method"].as_str()?;
            let at = request["path"].as_str()?;
            match method {
                "POST" if at == issues => {
                    let mut issue = shape.clone();
                    for (key, value) in [
                        ("id", json!(9_000_536_u64)),
                        ("number", json!(9001)),
                        ("title", request["body"]["title"].clone()),
                        ("body", request["body"]["body"].clone()),
                        ("labels", labels(&["agent:proposed".to_owned()])),
                        (
                            "url",
                            json!(format!("https://api.github.com/{issues}/9001")),
                        ),
                    ] {
                        issue[key] = value;
                    }
                    Some((issue, "not opened: shaped like issue #29".to_owned()))
                }
                "POST" if at.ends_with("/dependencies/blocked_by") => Some((
                    json!({}),
                    "not written: G4 wrote such a relation with 201".to_owned(),
                )),
                _ => panic!("refusing to write {method} {at} while recording the drain"),
            }
        })
        .amending(move |at, response| {
            at.starts_with(&list).then(|| {
                *response = json!([opened.clone()]);
                "recorded, then #31 shown open with its label, as when its gesture was made"
                    .to_owned()
            })
        })
    }

    #[test]
    #[ignore = "records against the live GitHub API"]
    fn record_github_drain_fixture() {
        let recorder = drain_recorder();
        let tracker = tracker(recorder.clone());
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check_drain(&tracker)));
        recorder.write(
            DRAIN,
            &format!(
                "{} on {SANDBOX}, recorded live, read-only: one list amended and the writes made \
                 up (see `synthesized`)",
                today()
            ),
        );
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    #[ignore = "records against the live GitHub API"]
    fn record_github_stage_fixture() {
        let (recorder, how) = recorder();
        let tracker = tracker(recorder.clone());
        let case = case();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check_stage(&tracker, &case.existing, &case.missing, THERE, BACK);
            check_reads(&tracker);
        }));
        recorder.write(STAGE, &format!("{} on {SANDBOX}, {how}", today()));
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }
}
