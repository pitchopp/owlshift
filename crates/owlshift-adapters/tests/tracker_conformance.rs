//! The tracker conformance suite: what every tracker adapter must do, run on
//! each of them. It asserts relative times only (order, last edit not before
//! creation), never absolute ones, so a tracker that stamps comments with its
//! own clock passes the same checks as one replayed from a recording.

mod support;

use std::fs;

use owlshift_adapters::tracker::linear::LinearTracker;
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_adapters::tracker::{Author, Capability, ErrorKind, Person, Tracker};
use owlshift_contracts::Priority;
use owlshift_contracts::ids::TicketId;

use support::{Recorder, Replay};

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
    /// Whether its assignee's account created it: the ticket's author is
    /// then that same account, which makes the description the decider's.
    created_by_assignee: bool,
    labels: &'static [&'static str],
    /// A ticket that does not exist.
    missing: TicketId,
    /// The body of the comment the suite posts.
    body: &'static str,
}

fn id(id: &str) -> TicketId {
    TicketId::new(id).unwrap()
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
    let by_assignee = matches!(
        &ticket.author,
        Author::Account(author) if ticket.assignee.as_ref() == Some(author)
    );
    assert_eq!(
        by_assignee, case.created_by_assignee,
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

const BODY: &str = "[owlshift] OWL-13 conformance check: a comment posted by the Linear \
                    adapter's fixture recorder · safe to delete.\n\nSecond line.";

#[test]
fn the_markdown_tracker_conforms() {
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

    let case = Case {
        existing: id("DEMO-1"),
        title: "Add a greeting",
        description_start: "Say hello",
        priority: Priority::Medium,
        assigned: true,
        created_by_assignee: true,
        labels: &["Feature", "Docs"],
        missing: id("DEMO-404"),
        body: BODY,
    };
    let tracker = MarkdownTracker::new(root.path());
    check(&tracker, &case);
    let maintainer = Person {
        id: "maintainer".to_owned(),
        name: "maintainer".to_owned(),
    };
    assert_eq!(
        Tracker::ticket(&tracker, &case.existing).unwrap().assignee,
        Some(maintainer)
    );
}

fn linear_case() -> Case {
    Case {
        existing: id("OWL-13"),
        title: "Add the Linear tracker adapter: read a ticket and post a comment",
        description_start: "**Why.**",
        priority: Priority::Medium,
        assigned: true,
        created_by_assignee: true,
        labels: &["Feature", "Adapters"],
        missing: id("OWL-99999"),
        body: BODY,
    }
}

const LINEAR_FIXTURE: &str = "conformance.json";

#[test]
fn the_linear_tracker_conforms() {
    let replay = Replay::new(LINEAR_FIXTURE);
    check(
        &LinearTracker::with_transport(replay.clone()),
        &linear_case(),
    );
    replay.assert_done();
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

fn today() -> String {
    jiff::Zoned::now().date().to_string()
}
