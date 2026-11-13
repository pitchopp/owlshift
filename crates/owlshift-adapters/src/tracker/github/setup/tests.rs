//! The answers below follow GitHub's documented shapes and G1's observed
//! statuses (2026-11-12). `GET user/<id>` and a create refused for a name
//! that exists are `synthesized`: this session's proxy refuses paths outside
//! a repository, so they wait on `docs/design/live-checks/owl-537.sh`.

use serde_json::{Value, json};

use super::super::tests::{Canned, tracker};
use super::IssuesAccess;
use crate::tracker::{AccountFact, ErrorKind};

const LABELS: &str = "GET repos/demo/project/labels?per_page=100&page=1";
const ISSUES: &str = "GET repos/demo/project/issues?per_page=1";

fn labels(names: &[&str]) -> Value {
    json!(
        names
            .iter()
            .map(|name| json!({ "name": name, "color": "ededed" }))
            .collect::<Vec<_>>()
    )
}

fn create(name: &str) -> String {
    format!(
        "POST repos/demo/project/labels {}",
        json!({ "name": name, "color": "ededed" })
    )
}

/// G1: before the installation accepts Issues, the list answers 403 with
/// GitHub's "Resource not accessible by integration"; each status the
/// fix depends on is told apart.
#[test]
fn the_issue_list_tells_what_keeps_the_issues_unread() {
    let refused = json!({ "message": "Resource not accessible by integration" });
    for (status, body, read) in [
        (200, json!([]), IssuesAccess::Granted),
        (
            403,
            refused.clone(),
            IssuesAccess::Refused("HTTP 403 Resource not accessible by integration".into()),
        ),
        (
            401,
            json!({ "message": "Bad credentials" }),
            IssuesAccess::TokenRefused("HTTP 401 Bad credentials".into()),
        ),
        (
            404,
            json!({ "message": "Not Found" }),
            IssuesAccess::NotFound("HTTP 404 Not Found".into()),
        ),
        (
            410,
            json!({ "message": "Issues are disabled for this repo" }),
            IssuesAccess::Disabled("HTTP 410 Issues are disabled for this repo".into()),
        ),
    ] {
        let canned = Canned::default().on(ISSUES, status, body);
        assert_eq!(tracker(&canned).issues_access().unwrap(), read, "{status}");
    }
    let canned = Canned::default().on(ISSUES, 500, json!({}));
    assert_eq!(
        tracker(&canned).issues_access().unwrap_err().kind,
        ErrorKind::Other
    );
}

/// Only the labels the repository lacks are created, compared without
/// regard to case, each once; a second run, the labels there, creates none.
#[test]
fn only_the_missing_labels_are_created_and_a_second_run_creates_none() {
    let wanted = [
        "agent:ready",
        "agent:review",
        "agent:proposed",
        "AGENT:READY",
    ];
    let canned = Canned::default()
        .on(LABELS, 200, labels(&["bug", "Agent:Review"]))
        .on(
            &create("agent:ready"),
            201,
            json!({ "name": "agent:ready" }),
        )
        .on(
            &create("agent:proposed"),
            201,
            json!({ "name": "agent:proposed" }),
        );
    let created = tracker(&canned).create_missing_labels(&wanted).unwrap();
    assert_eq!(created, ["agent:ready", "agent:proposed"]);
    assert_eq!(
        canned.calls(),
        [
            LABELS.to_owned(),
            create("agent:ready"),
            create("agent:proposed")
        ]
    );

    let again = Canned::default().on(
        LABELS,
        200,
        labels(&["bug", "Agent:Review", "agent:ready", "agent:proposed"]),
    );
    assert!(
        tracker(&again)
            .create_missing_labels(&wanted)
            .unwrap()
            .is_empty()
    );
    assert_eq!(again.calls(), [LABELS]);
}

/// A create refused as invalid is read again: a label made meanwhile is
/// taken; one still missing fails naming it. A refused token is not read
/// again and keeps its own error, which names Issues. (synthesized)
#[test]
fn a_refused_create_is_taken_only_when_the_label_is_there_after_all() {
    let exists = json!({
        "message": "Validation Failed",
        "errors": [{ "resource": "Label", "code": "already_exists", "field": "name" }]
    });
    let canned = Canned::default()
        .on(LABELS, 200, labels(&[]))
        .on(&create("agent:ready"), 422, exists.clone())
        .on(LABELS, 200, labels(&["agent:ready"]));
    assert!(
        tracker(&canned)
            .create_missing_labels(&["agent:ready"])
            .unwrap()
            .is_empty()
    );

    let canned =
        Canned::default()
            .on(LABELS, 200, labels(&[]))
            .on(&create("agent:ready"), 422, exists);
    let error = tracker(&canned)
        .create_missing_labels(&["agent:ready"])
        .unwrap_err();
    assert!(error.message.contains("\"agent:ready\""), "{error:?}");

    let canned = Canned::default().on(LABELS, 200, labels(&[])).on(
        &create("agent:ready"),
        403,
        json!({ "message": "Resource not accessible by integration" }),
    );
    let error = tracker(&canned)
        .create_missing_labels(&["agent:ready"])
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unauthorized);
    assert!(
        error.message.contains("Issues (read and write)"),
        "{error:?}"
    );
    assert_eq!(canned.calls().len(), 2);
}

/// The signed-in account's numeric id; each admitter asked by its id: a
/// person, a bot, an account GitHub does not know, and a name that is no
/// id, which is not asked. (`user/<id>` synthesized)
#[test]
fn the_signed_in_account_and_the_admitters_are_read_by_id() {
    let canned = Canned::default()
        .on(
            "GET user",
            200,
            json!({ "login": "ann", "id": 1, "type": "User" }),
        )
        .on(
            "GET user/1",
            200,
            json!({ "login": "ann", "id": 1, "type": "User" }),
        )
        .on(
            "GET user/9",
            200,
            json!({ "login": "a-bot[bot]", "id": 9, "type": "Bot" }),
        )
        .on("GET user/404", 404, json!({ "message": "Not Found" }));
    let tracker = tracker(&canned);
    assert_eq!(tracker.signed_in().unwrap(), 1);
    let known = tracker.accounts(&["1", "9", "404", "ann"]).unwrap();
    assert_eq!(
        known,
        [
            AccountFact {
                id: "1".into(),
                app: false,
                active: true
            },
            AccountFact {
                id: "9".into(),
                app: true,
                active: true
            },
        ]
    );
    assert!(!canned.calls().iter().any(|c| c.contains("ann")));
}
