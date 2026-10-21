//! The Linear adapter on recorded exchanges: what the conformance suite does
//! not cover (an assigned ticket, pagination, a rejected key), the hygiene of
//! the fixtures, and the recorder of these fixtures.

mod support;

use std::fs;

use owlshift_adapters::tracker::linear::{ApiKey, LinearTracker};
use owlshift_adapters::tracker::{
    Author, Blocker, Column, ErrorKind, Gesture, Person, StateType, Tracker,
};
use owlshift_contracts::Priority;
use owlshift_contracts::config::Admit;
use owlshift_contracts::ids::TicketId;

use support::{Recorder, Replay};

const TICKET: &str = "ticket-owl-11.json";
const COMMENTS: &str = "comments-owl-11-paged.json";
const UNAUTHORIZED: &str = "unauthorized.json";
const TEAM_STATES: &str = "team-states-owl.json";
const ADMITTED_TODO: &str = "admitted-todo.json";
const ADMITTED_PAGED: &str = "admitted-agent-paged.json";
const GESTURES: &str = "gestures.json";
const LINKS: &str = "links.json";

/// The ready column of the drain's exit gate, for each kind of gesture.
fn todo_by_state() -> Admit {
    Admit::State("Todo".to_owned())
}

fn agent_label() -> Admit {
    Admit::Label("agent".to_owned())
}

/// The gestures recorded, in order: a label added by an account (OWL-129),
/// a label set at creation and a move by an account (OWL-179), a ticket
/// created in the state (OWL-249).
fn gestures() -> [(TicketId, Admit); 4] {
    let id = |id: &str| TicketId::new(id).unwrap();
    [
        (
            id("OWL-129"),
            Admit::Label("zone:backend/billing".to_owned()),
        ),
        (id("OWL-179"), agent_label()),
        (id("OWL-179"), Admit::State("Canceled".to_owned())),
        (id("OWL-249"), todo_by_state()),
    ]
}

/// Team `OWL`'s Todo on 2026-10-13, a state gesture: the P4 tickets, each
/// with its `blocks` relations and their state types, read as one page.
/// Every ticket reads as [`Tracker::ticket`] would read it.
#[test]
fn lists_the_ready_column_with_its_blockers() {
    let replay = Replay::new(ADMITTED_TODO);
    let listed = LinearTracker::with_transport(replay.clone())
        .admitted(&Column {
            team: Some("owl"),
            ready: "Todo",
            admit: &todo_by_state(),
        })
        .unwrap();
    replay.assert_done();
    assert!(listed.len() >= 2, "{listed:?}");
    let owl_155 = listed
        .iter()
        .find(|l| l.ticket.id.as_str() == "OWL-155")
        .expect("OWL-155 in Todo");
    assert_eq!(owl_155.ticket.priority, Priority::Medium);
    assert!(owl_155.created_at.is_some());
    assert!(matches!(owl_155.ticket.author, Author::Other { .. }));
    let blocker = |id: &str| Blocker {
        id: TicketId::new(id).unwrap(),
        state: Some(StateType::Started),
    };
    assert_eq!(owl_155.blockers, [blocker("OWL-246"), blocker("OWL-245")]);
    let types: std::collections::HashSet<_> = listed
        .iter()
        .flat_map(|l| l.blockers.iter().map(|b| b.state))
        .collect();
    assert!(types.contains(&Some(StateType::Completed)), "{types:?}");
    assert!(types.contains(&Some(StateType::Unstarted)), "{types:?}");
}

/// Intake's links (OWL-416), as `links.json` holds them for each ticket it
/// asked: every linked issue with its title and its state's name, none
/// listed twice in a list, and at least one link in the fixture.
#[test]
fn reads_a_tickets_links_both_ways() {
    let fixture = support::load(&support::fixtures_dir(), LINKS);
    let replay = Replay::new(LINKS);
    let tracker = LinearTracker::with_transport(replay.clone());
    let mut seen = 0;
    for exchange in &fixture.exchanges {
        let id = TicketId::new(exchange.request["variables"]["id"].as_str().unwrap()).unwrap();
        let links = tracker.links(&id).unwrap();
        for list in [&links.blocked_by, &links.blocks, &links.related] {
            for (n, linked) in list.iter().enumerate() {
                assert!(linked.title.as_deref().is_some_and(|t| !t.is_empty()));
                assert!(linked.state.as_ref().is_some_and(|s| !s.name.is_empty()));
                assert!(list[..n].iter().all(|earlier| earlier.id != linked.id));
            }
            seen += list.len();
        }
    }
    replay.assert_done();
    assert!(seen > 0, "{LINKS} holds no link");
}

/// A label gesture, read two a page to the last: the five P2 exit gate
/// tickets labelled `agent`, in the Canceled state since Todo held none.
#[test]
fn lists_a_labelled_column_across_pages() {
    let replay = Replay::new(ADMITTED_PAGED);
    let listed = LinearTracker::with_transport(replay.clone())
        .with_page(2)
        .admitted(&Column {
            team: Some("OWL"),
            ready: "Canceled",
            admit: &agent_label(),
        })
        .unwrap();
    replay.assert_done();
    let ids: Vec<&str> = listed.iter().map(|l| l.ticket.id.as_str()).collect();
    assert_eq!(ids, ["OWL-179", "OWL-178", "OWL-175", "OWL-174", "OWL-173"]);
    for listed in &listed {
        assert!(listed.ticket.labels.iter().any(|l| l == "agent"));
    }
}

/// Who made each gesture, as recorded: the account that added the label,
/// the creator for a label set at creation, the account that moved the
/// ticket, the creator of a ticket created in the state; the maintainer's
/// account each time.
#[test]
fn reads_who_made_the_gesture() {
    let replay = Replay::new(GESTURES);
    let tracker = LinearTracker::with_transport(replay.clone());
    let read: Vec<Gesture> = gestures()
        .iter()
        .map(|(id, admit)| tracker.gesture(id, admit).unwrap())
        .collect();
    replay.assert_done();
    for gesture in &read {
        assert_eq!(
            gesture.account,
            Author::Account(maintainer()),
            "{gesture:?}"
        );
    }
    let at = |gesture: &Gesture| gesture.at.unwrap().to_string();
    // OWL-129's label came after its creation, OWL-179's with it.
    assert_eq!(at(&read[0]), "2026-10-02T14:21:59.058Z");
    assert_eq!(at(&read[1]), "2026-10-07T08:44:52.263Z");
    assert_eq!(at(&read[2]), "2026-10-07T09:14:14.764Z");
    assert_eq!(at(&read[3]), "2026-10-13T09:33:46.135Z");
}

/// The team's states as `owlshift doctor` reads them (OWL-147): the nine of
/// `OWL` in one page, then a key no team has, sent upper case as written in
/// any case.
#[test]
fn reads_a_teams_workflow_states() {
    let replay = Replay::new(TEAM_STATES);
    let tracker = LinearTracker::with_transport(replay.clone());
    let states = tracker.team_states("OWL").unwrap();
    assert_eq!(
        states,
        [
            "In Review",
            "Needs Input",
            "Triage",
            "Duplicate",
            "Done",
            "Backlog",
            "Todo",
            "Canceled",
            "In Progress"
        ]
    );
    let unknown = tracker.team_states("nope").unwrap_err();
    replay.assert_done();
    assert_eq!(unknown.kind, ErrorKind::NotFound, "{unknown}");
    assert!(unknown.message.contains("\"NOPE\""), "{unknown}");
}

fn owl_11() -> TicketId {
    TicketId::new("OWL-11").unwrap()
}

fn maintainer() -> Person {
    Person {
        id: "00000000-0000-4000-8000-000000000001".to_owned(),
        name: "person-1".to_owned(),
    }
}

#[test]
fn reads_an_assigned_ticket() {
    let replay = Replay::new(TICKET);
    let ticket = LinearTracker::with_transport(replay.clone())
        .ticket(&owl_11())
        .unwrap();
    replay.assert_done();
    assert_eq!(ticket.id, owl_11());
    assert!(
        ticket.title.starts_with("Build the test harness"),
        "{}",
        ticket.title
    );
    assert!(!ticket.description.is_empty());
    assert_eq!(ticket.priority, Priority::High);
    assert_eq!(ticket.assignee, Some(maintainer()));
    // Created by the assignee's account, which a holder of its API key
    // could be (OWL-74): the creator is named, never an account.
    assert_eq!(
        ticket.author,
        Author::Other {
            name: maintainer().name
        }
    );
    assert!(!ticket.labels.is_empty());
}

/// Recorded one comment per page, so two pages; Linear sends the newest
/// first and the adapter returns the oldest first.
#[test]
fn reads_comments_across_pages_oldest_first() {
    let replay = Replay::new(COMMENTS);
    let comments = LinearTracker::with_transport(replay.clone())
        .with_comment_page(1)
        .comments(&owl_11())
        .unwrap();
    replay.assert_done();
    assert_eq!(comments.len(), 2);
    assert!(comments[0].created_at < comments[1].created_at);
    assert!(comments[0].body.starts_with("Pull request opened"));
    assert!(comments[1].body.starts_with("Merged into main"));
    for comment in &comments {
        assert_eq!(comment.author, Author::Account(maintainer()));
        assert_eq!(comment.edited_at, None);
    }
}

#[test]
fn a_rejected_key_is_unauthorized() {
    let replay = Replay::new(UNAUTHORIZED);
    let error = LinearTracker::with_transport(replay.clone())
        .ticket(&owl_11())
        .unwrap_err();
    replay.assert_done();
    assert_eq!(error.kind, ErrorKind::Unauthorized, "{error}");
}

/// No fixture may hold a key, a header, an e-mail address or an account's
/// real identity.
#[test]
fn fixtures_hold_no_credential_or_personal_data() {
    for entry in fs::read_dir(support::fixtures_dir()).unwrap() {
        let path = entry.unwrap().path();
        let text = fs::read_to_string(&path).unwrap();
        for forbidden in ["lin_api_", "uthorization", "email", "@"] {
            assert!(
                !text.contains(forbidden),
                "{} contains {forbidden:?}",
                path.display()
            );
        }
        let fixture: support::Fixture = serde_json::from_str(&text).unwrap();
        for exchange in &fixture.exchanges {
            assert_no_real_account(&exchange.response, &path);
        }
    }
}

/// Every account left in a fixture is a pseudonym.
fn assert_no_real_account(value: &serde_json::Value, path: &std::path::Path) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(name) = map.get("displayName") {
                let name = name.as_str().unwrap_or_default();
                assert!(name.starts_with("person-"), "{}: {name:?}", path.display());
            }
            map.values().for_each(|v| assert_no_real_account(v, path));
        }
        serde_json::Value::Array(items) => {
            items.iter().for_each(|v| assert_no_real_account(v, path))
        }
        _ => {}
    }
}

/// Records the fixtures above against the live API. Read only: nothing is
/// written to Linear.
#[test]
#[ignore = "records against the live Linear API"]
fn record_read_fixtures() {
    let key = support::live_key();
    support::assert_owlshift_workspace(&key);
    let recorded = format!(
        "{} on the Owlshift workspace, recorded live",
        jiff::Zoned::now().date()
    );

    let recorder = Recorder::new(&key);
    LinearTracker::with_transport(recorder.clone())
        .ticket(&owl_11())
        .unwrap();
    recorder.write(TICKET, &recorded);

    let recorder = Recorder::new(&key);
    LinearTracker::with_transport(recorder.clone())
        .with_comment_page(1)
        .comments(&owl_11())
        .unwrap();
    recorder.write(COMMENTS, &recorded);

    // A well-formed key that Linear does not know.
    let rejected = ApiKey::new(format!("lin_api_{}", "0".repeat(40)));
    let recorder = Recorder::new(&rejected);
    let error = LinearTracker::with_transport(recorder.clone())
        .ticket(&owl_11())
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unauthorized, "{error}");
    recorder.write(UNAUTHORIZED, &recorded);

    let recorder = Recorder::new(&key);
    let tracker = LinearTracker::with_transport(recorder.clone());
    tracker.team_states("OWL").unwrap();
    let unknown = tracker.team_states("NOPE").unwrap_err();
    assert_eq!(unknown.kind, ErrorKind::NotFound, "{unknown}");
    recorder.write(TEAM_STATES, &recorded);
}

/// Records the drain's fixtures (OWL-246) against the live API. Read only.
#[test]
#[ignore = "records against the live Linear API"]
fn record_drain_fixtures() {
    let key = support::live_key();
    support::assert_owlshift_workspace(&key);
    let recorded = format!(
        "{} on the Owlshift workspace, recorded live",
        jiff::Zoned::now().date()
    );

    let recorder = Recorder::new(&key);
    LinearTracker::with_transport(recorder.clone())
        .admitted(&Column {
            team: Some("owl"),
            ready: "Todo",
            admit: &todo_by_state(),
        })
        .unwrap();
    recorder.write(ADMITTED_TODO, &recorded);

    let recorder = Recorder::new(&key);
    LinearTracker::with_transport(recorder.clone())
        .with_page(2)
        .admitted(&Column {
            team: Some("OWL"),
            ready: "Canceled",
            admit: &agent_label(),
        })
        .unwrap();
    recorder.write(ADMITTED_PAGED, &recorded);

    let recorder = Recorder::new(&key);
    let tracker = LinearTracker::with_transport(recorder.clone());
    for (id, admit) in gestures() {
        tracker.gesture(&id, &admit).unwrap();
    }
    recorder.write(GESTURES, &recorded);
}

/// Records intake's links (OWL-416) against the live API, read only, for
/// each ticket `OWLSHIFT_RECORD_LINKS_TICKETS` names (comma-separated),
/// which together should hold a `blocks` relation each way and a `related`
/// one. Prints the date, then per ticket how many links each direction has
/// and their state types: never the key, nor a title.
#[test]
#[ignore = "records against the live Linear API; needs OWLSHIFT_RECORD_LINKS_TICKETS"]
fn record_links_fixture() {
    let tickets = std::env::var("OWLSHIFT_RECORD_LINKS_TICKETS")
        .expect("OWLSHIFT_RECORD_LINKS_TICKETS names the issues to read, comma-separated");
    let key = support::live_key();
    support::assert_owlshift_workspace(&key);
    let date = jiff::Zoned::now().date();
    let recorder = Recorder::new(&key);
    let tracker = LinearTracker::with_transport(recorder.clone());
    println!("links check on {date}, Owlshift workspace");
    for id in tickets.split(',').map(str::trim) {
        let links = tracker.links(&TicketId::new(id).unwrap()).unwrap();
        let shape = |list: &[owlshift_adapters::tracker::Linked]| {
            let kinds: Vec<String> = list
                .iter()
                .map(|l| match l.state.as_ref().and_then(|s| s.kind) {
                    Some(kind) => format!("{kind:?}"),
                    None => "untyped".to_owned(),
                })
                .collect();
            format!("{} [{}]", list.len(), kinds.join(", "))
        };
        println!(
            "{id}: blocked_by {}; blocks {}; related {}",
            shape(&links.blocked_by),
            shape(&links.blocks),
            shape(&links.related)
        );
    }
    recorder.write(
        LINKS,
        &format!("{date} on the Owlshift workspace, recorded live"),
    );
}
