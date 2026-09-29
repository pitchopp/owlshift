//! The Linear adapter on recorded exchanges: what the conformance suite does
//! not cover (an assigned ticket, pagination, a rejected key), the hygiene of
//! the fixtures, and the recorder of these fixtures.

mod support;

use std::fs;

use owlshift_adapters::tracker::linear::{ApiKey, LinearTracker};
use owlshift_adapters::tracker::{Author, ErrorKind, Person, Tracker};
use owlshift_contracts::Priority;
use owlshift_contracts::ids::TicketId;

use support::{Recorder, Replay};

const TICKET: &str = "ticket-owl-11.json";
const COMMENTS: &str = "comments-owl-11-paged.json";
const UNAUTHORIZED: &str = "unauthorized.json";

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
}
