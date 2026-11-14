use serde_json::{Value, json};

use owlshift_contracts::config::Admit;

use super::super::tests::{Canned, ISSUE, comment, id, issue, tracker};
use crate::tracker::{
    Author, Blocker, Column, ErrorKind, PROPOSED_LABEL, Proposal, StateType, Tracker,
};

const ISSUES: &str = "repos/demo/project/issues";
const READY: &str = "repos/demo/project/issues?state=open&labels=agent%3Aready&per_page=100&page=1";

/// Issue `n` as a list gives it: open, this repository's, with `labels`.
fn listed(n: u64, labels: &[&str]) -> Value {
    json!({
        "id": 1000 + n,
        "number": n,
        "url": format!("https://api.github.com/repos/demo/project/issues/{n}"),
        "repository_url": "https://api.github.com/repos/demo/project",
        "title": format!("Issue {n}"),
        "body": format!("Body {n}"),
        "user": { "login": "ann", "id": 1, "type": "User" },
        "assignees": [{ "login": "bea", "id": 2, "type": "User" }],
        "labels": labels.iter().map(|name| json!({ "name": name })).collect::<Vec<_>>(),
        "state": "open",
        "state_reason": null,
        "created_at": format!("2026-11-12T15:00:{n:02}Z"),
    })
}

/// [`listed`], closed with `reason`.
fn closed(n: u64, reason: Value) -> Value {
    let mut issue = listed(n, &[]);
    issue["state"] = json!("closed");
    issue["state_reason"] = reason;
    issue
}

fn blocked_by(n: u64) -> String {
    format!("GET {ISSUES}/{n}/dependencies/blocked_by?per_page=100&page=1")
}

fn column(admit: &Admit) -> Column<'_> {
    Column {
        team: None,
        ready: "agent:ready",
        admit,
    }
}

fn label() -> Admit {
    Admit::Label("agent:ready".to_owned())
}

fn event(kind: &str, label: &str, at: &str, actor: Value) -> Value {
    json!({
        "id": 1, "event": kind, "actor": actor, "label": { "name": label },
        "created_at": at, "performed_via_github_app": null,
    })
}

fn ann() -> Value {
    json!({ "login": "ann", "id": 1, "type": "User" })
}

fn events(n: u64) -> String {
    format!("GET {ISSUES}/{n}/events?per_page=100&page=1")
}

/// The ready column lists the open issues carrying the ready label, pull
/// requests and repeats left out, each with its blockers read as G4 gave
/// them; a second admission label is asked with it.
#[test]
fn the_ready_column_lists_issues_with_their_blockers() {
    let mut pull = listed(9, &["agent:ready"]);
    pull["pull_request"] = json!({ "merged_at": null });
    let mut merged = closed(24, Value::Null);
    merged["pull_request"] = json!({ "merged_at": "2026-11-12T15:00:00Z" });
    let mut unmerged = closed(25, Value::Null);
    unmerged["pull_request"] = json!({ "merged_at": null });
    let mut foreign = listed(3, &[]);
    foreign["repository_url"] = json!("https://api.github.com/repos/other.org/lib");
    let canned = Canned::default()
        .on(
            &format!("GET {READY}"),
            200,
            json!([
                listed(1, &["agent:ready"]),
                pull,
                listed(2, &["Agent:Ready"]),
                listed(1, &["agent:ready"])
            ]),
        )
        .on(
            &blocked_by(1),
            200,
            json!([
                listed(20, &[]),
                closed(21, json!("completed")),
                closed(22, json!("not_planned")),
                closed(23, json!("duplicate")),
                merged,
                unmerged,
                foreign,
                closed(26, Value::Null),
                listed(20, &[]),
            ]),
        )
        .on(&blocked_by(2), 200, json!([]));
    let found = tracker(&canned).admitted(&column(&label())).unwrap();
    let ids: Vec<&str> = found.iter().map(|l| l.ticket.id.as_str()).collect();
    assert_eq!(ids, ["1", "2"]);
    assert_eq!(found[0].ticket.title, "Issue 1");
    assert_eq!(
        found[0].created_at.unwrap().to_string(),
        "2026-11-12T15:00:01Z"
    );
    let blockers: Vec<(String, Option<StateType>)> = found[0]
        .blockers
        .iter()
        .map(|Blocker { id, state }| (id.as_str().to_owned(), *state))
        .collect();
    assert_eq!(
        blockers,
        [
            ("20".to_owned(), None),
            ("21".to_owned(), Some(StateType::Completed)),
            ("22".to_owned(), Some(StateType::Canceled)),
            ("23".to_owned(), Some(StateType::Duplicate)),
            ("24".to_owned(), Some(StateType::Completed)),
            ("25".to_owned(), Some(StateType::Canceled)),
            ("other_org-lib-3".to_owned(), None),
            ("26".to_owned(), Some(StateType::Completed)),
        ]
    );

    // An admission label of its own is asked with the ready label.
    let canned = Canned::default().on(
        &format!("GET {ISSUES}?state=open&labels=agent%3Aready,needs%20owl&per_page=100&page=1"),
        200,
        json!([]),
    );
    let admit = Admit::Label("needs owl".to_owned());
    assert!(
        tracker(&canned)
            .admitted(&column(&admit))
            .unwrap()
            .is_empty()
    );

    // The ready state is the ready label.
    let canned = Canned::default().on(&format!("GET {READY}"), 200, json!([]));
    let admit = Admit::State("agent:ready".to_owned());
    assert!(
        tracker(&canned)
            .admitted(&column(&admit))
            .unwrap()
            .is_empty()
    );
}

/// A blocker list that cannot be read fails the listing: never a ticket
/// with no blocker. `delegation` is refused before any request.
#[test]
fn a_listing_whose_blockers_are_unread_fails() {
    let canned = Canned::default()
        .on(
            &format!("GET {READY}"),
            200,
            json!([listed(1, &["agent:ready"])]),
        )
        .on(&blocked_by(1), 502, json!({ "message": "Server Error" }));
    assert!(tracker(&canned).admitted(&column(&label())).is_err());

    let canned = Canned::default();
    assert!(
        tracker(&canned)
            .admitted(&column(&Admit::Delegation))
            .is_err()
    );
    assert!(
        tracker(&canned)
            .gesture(&id("7"), &Admit::Delegation)
            .is_err()
    );
    assert!(canned.calls().is_empty());
}

/// The gesture is the last `labeled` event of the label that no
/// `unlabeled` follows: its actor's id and its time. One made through a
/// GitHub App, or by a bot, is no account's; none standing is an error.
#[test]
fn the_gesture_is_the_last_label_event_standing() {
    let bot = json!({ "login": "a-bot[bot]", "id": 9, "type": "Bot" });
    let bea = json!({ "login": "bea", "id": 2, "type": "User" });
    let mut by_app = event(
        "labeled",
        "agent:ready",
        "2026-11-12T15:30:00Z",
        bea.clone(),
    );
    by_app["performed_via_github_app"] = json!({ "slug": "owl-shift" });
    for (list, read) in [
        (
            vec![
                event("labeled", "agent:ready", "2026-11-12T15:00:00Z", ann()),
                event("labeled", "Bug", "2026-11-12T15:05:00Z", bea.clone()),
                event("unlabeled", "Agent:Ready", "2026-11-12T15:10:00Z", ann()),
                event(
                    "labeled",
                    "Agent:Ready",
                    "2026-11-12T15:20:00Z",
                    bea.clone(),
                ),
            ],
            Ok(("2", "2026-11-12T15:20:00Z")),
        ),
        (
            vec![
                event("labeled", "agent:ready", "2026-11-12T15:00:00Z", ann()),
                event("unlabeled", "agent:ready", "2026-11-12T15:10:00Z", ann()),
            ],
            Err("put the label on the issue again"),
        ),
        (
            vec![
                event("labeled", "agent:ready", "2026-11-12T15:00:00Z", ann()),
                by_app,
            ],
            Err("bea through the GitHub App owl-shift"),
        ),
        (
            vec![event("labeled", "agent:ready", "2026-11-12T15:00:00Z", bot)],
            Err("a-bot[bot]"),
        ),
    ] {
        let canned = Canned::default().on(&events(7), 200, json!(list)).on(
            &last_edited(7),
            200,
            edited(None),
        );
        let gesture = tracker(&canned).gesture(&id("7"), &label());
        match (read, gesture) {
            (Ok((account, at)), Ok(gesture)) => {
                let Author::Account(person) = gesture.account else {
                    panic!("{gesture:?}");
                };
                assert_eq!(person.id, account);
                assert_eq!(gesture.at.unwrap().to_string(), at);
                assert_eq!(gesture.edited_at, None);
                // A person's gesture reads the issue's last edit too.
                assert_eq!(canned.calls(), [events(7), last_edited(7)]);
            }
            (Err(name), Ok(gesture)) => {
                assert_eq!(
                    gesture.account,
                    Author::Other {
                        name: name.to_owned()
                    }
                );
                // One that never admits reads the events alone.
                assert_eq!(gesture.edited_at, None);
                assert_eq!(canned.calls(), [events(7)]);
            }
            (Err(message), Err(error)) => {
                assert!(error.message.contains(message), "{error}");
                assert_eq!(canned.calls(), [events(7)]);
            }
            (want, got) => panic!("{want:?}: {got:?}"),
        }
    }
}

/// The GraphQL read of issue `n`'s last edit, by its variables.
fn last_edited(n: u64) -> String {
    format!(
        "GRAPHQL {}",
        json!({ "owner": "demo", "name": "project", "number": n })
    )
}

/// GraphQL's answer of an issue last edited at `at`, never when `None`.
fn edited(at: Option<&str>) -> Value {
    json!({ "data": { "repository": { "issue": { "lastEditedAt": at } } } })
}

/// A person's gesture carries the issue's last text edit (OWL-557): the
/// later of its last `renamed` event (a title edit) and GraphQL's
/// `lastEditedAt` (a description edit), as the live check of 2026-11-14
/// gave them; none when never edited. The runner compares it with the
/// gesture's time. An issue GraphQL does not answer is an error.
#[test]
fn a_persons_gesture_carries_the_last_edit_of_the_text() {
    let labeled = || event("labeled", "agent:ready", "2026-11-14T10:33:51Z", ann());
    let mut renamed = event("renamed", "", "2026-11-14T10:34:19Z", ann());
    renamed["label"] = Value::Null;
    renamed["rename"] = json!({ "from": "Old", "to": "New" });
    for (list, last, want) in [
        // Never edited.
        (vec![labeled()], None, None),
        // The description edited before the label.
        (
            vec![labeled()],
            Some("2026-11-14T10:33:44Z"),
            Some("2026-11-14T10:33:44Z"),
        ),
        // The description edited after it.
        (
            vec![labeled()],
            Some("2026-11-14T10:34:12Z"),
            Some("2026-11-14T10:34:12Z"),
        ),
        // The title renamed after the description's edit: the later.
        (
            vec![labeled(), renamed.clone()],
            Some("2026-11-14T10:34:12Z"),
            Some("2026-11-14T10:34:19Z"),
        ),
    ] {
        let canned = Canned::default().on(&events(7), 200, json!(list)).on(
            &last_edited(7),
            200,
            edited(last),
        );
        let gesture = tracker(&canned).gesture(&id("7"), &label()).unwrap();
        assert_eq!(gesture.at.unwrap().to_string(), "2026-11-14T10:33:51Z");
        assert_eq!(
            gesture.edited_at.map(|at| at.to_string()).as_deref(),
            want,
            "{last:?}"
        );
    }

    let canned = Canned::default()
        .on(&events(7), 200, json!([labeled()]))
        .on(
            &last_edited(7),
            200,
            json!({ "data": { "repository": { "issue": null } } }),
        );
    let error = tracker(&canned).gesture(&id("7"), &label()).unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotFound, "{error}");
}

/// A full page of events reads the next one.
#[test]
fn every_page_of_events_is_read() {
    let mut full: Vec<Value> = (0..100)
        .map(|_| event("labeled", "Bug", "2026-11-12T15:00:00Z", ann()))
        .collect();
    full[0] = event("labeled", "agent:ready", "2026-11-12T15:00:00Z", ann());
    let canned = Canned::default().on(&events(7), 200, json!(full)).on(
        &format!("GET {ISSUES}/7/events?per_page=100&page=2"),
        200,
        json!([event(
            "unlabeled",
            "agent:ready",
            "2026-11-12T15:01:00Z",
            ann()
        )]),
    );
    assert!(tracker(&canned).gesture(&id("7"), &label()).is_err());
}

/// A ticket's state: a pull request's number is not found; an open issue
/// has no type; the closing reason says the rest.
#[test]
fn a_state_type_reads_the_closing_reason() {
    let mut pull = issue(&[], "open");
    pull["pull_request"] = json!({ "merged_at": null });
    let canned = Canned::default().on(&format!("GET {ISSUE}"), 200, pull);
    let error = tracker(&canned).state_type(&id("7")).unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotFound);
    for (state, reason, read) in [
        ("open", Value::Null, None),
        ("open", json!("reopened"), None),
        ("closed", json!("completed"), Some(StateType::Completed)),
        ("closed", json!("not_planned"), Some(StateType::Canceled)),
        ("closed", json!("duplicate"), Some(StateType::Duplicate)),
        ("closed", Value::Null, Some(StateType::Completed)),
    ] {
        let mut answer = issue(&[], state);
        answer["state_reason"] = reason.clone();
        let canned = Canned::default().on(&format!("GET {ISSUE}"), 200, answer);
        assert_eq!(
            tracker(&canned).state_type(&id("7")).unwrap(),
            read,
            "{state} {reason}"
        );
    }
}

/// Relations and links read the dependency lists; a relation already
/// there is not written again, a new one by the blocker's database id.
#[test]
fn relations_links_and_a_block_written_once() {
    let canned = Canned::default()
        .on(&format!("GET {ISSUE}"), 200, issue(&[], "open"))
        .on(&blocked_by(7), 200, json!([closed(3, json!("completed"))]))
        .on(
            &format!("GET {ISSUES}/7/dependencies/blocking?per_page=100&page=1"),
            200,
            json!([listed(8, &[])]),
        );
    let relations = tracker(&canned).relations(&id("7")).unwrap();
    assert_eq!(relations.id.as_str(), "7");
    assert_eq!(relations.state, None);
    assert_eq!(relations.blockers.len(), 1);
    let links = tracker(&canned).links(&id("7")).unwrap();
    assert_eq!(links.blocked_by[0].id.as_str(), "3");
    assert_eq!(links.blocked_by[0].state.as_ref().unwrap().name, "closed");
    assert_eq!(links.blocks[0].title.as_deref(), Some("Issue 8"));
    assert!(links.related.is_empty());

    // Already there: nothing written.
    tracker(&canned).block(&id("3"), &id("7")).unwrap();
    assert!(!canned.calls().iter().any(|c| c.starts_with("POST")));

    let canned = Canned::default()
        .on(&blocked_by(7), 200, json!([]))
        .on(&format!("GET {ISSUES}/4"), 200, listed(4, &[]))
        .on(
            &format!(r#"POST {ISSUES}/7/dependencies/blocked_by {{"issue_id":1004}}"#),
            201,
            json!({}),
        );
    tracker(&canned).block(&id("4"), &id("7")).unwrap();
    assert_eq!(
        canned.calls().last().unwrap(),
        &format!(r#"POST {ISSUES}/7/dependencies/blocked_by {{"issue_id":1004}}"#)
    );
}

fn proposal<'a>(parent: &'a owlshift_contracts::ids::TicketId, ready: &'a str) -> Proposal<'a> {
    Proposal {
        parent,
        title: "Say goodbye",
        description: "Why\n\n[owlshift] PROPOSAL · 7 · fingerprint abc",
        blocked_by_parent: true,
        ready,
    }
}

/// A proposal is an issue with the proposals' label, blocked by its
/// parent; a relation not written is a problem, the issue filed. A ready
/// label that is the proposals' one is refused before any request.
#[test]
fn a_proposal_is_an_issue_with_its_label_blocked_by_its_parent() {
    let canned = Canned::default();
    let parent = id("7");
    let error = tracker(&canned)
        .propose(&proposal(&parent, PROPOSED_LABEL))
        .unwrap_err();
    assert!(error.message.contains("refused"), "{error}");
    assert!(canned.calls().is_empty());

    let open = format!(
        r#"POST {ISSUES} {{"body":"Why\n\n[owlshift] PROPOSAL · 7 · fingerprint abc","labels":["agent:proposed"],"title":"Say goodbye"}}"#
    );
    let mut parent_answer = issue(&[], "open");
    parent_answer["id"] = json!(1007);
    for (status, problems) in [(201, 0), (403, 1)] {
        let canned = Canned::default()
            .on(&format!("GET {ISSUE}"), 200, parent_answer.clone())
            .on(&open, 201, listed(40, &["agent:proposed"]))
            .on(
                &format!(r#"POST {ISSUES}/40/dependencies/blocked_by {{"issue_id":1007}}"#),
                status,
                json!({ "message": "x" }),
            );
        let filed = tracker(&canned)
            .propose(&proposal(&parent, "agent:ready"))
            .unwrap();
        assert_eq!(filed.id.as_str(), "40");
        assert_eq!(filed.state, "agent:proposed");
        assert_eq!(filed.problems.len(), problems, "{:?}", filed.problems);
    }
}

/// Proposals are found among the issues carrying the label, open or
/// closed, by title in any case or by marker; pull requests are not.
#[test]
fn proposals_are_found_by_title_or_marker_open_or_closed() {
    let mut by_title = closed(41, json!("completed"));
    by_title["title"] = json!("SAY GOODBYE");
    let mut by_marker = listed(42, &[]);
    by_marker["body"] = json!("text\n\n[owlshift] PROPOSAL · 7 · fingerprint abc");
    let mut pull = listed(43, &[]);
    pull["title"] = json!("Say goodbye");
    pull["pull_request"] = json!({ "merged_at": null });
    let canned = Canned::default().on(
        &format!("GET {ISSUES}?state=all&labels=agent%3Aproposed&per_page=100&page=1"),
        200,
        json!([by_title, by_marker, pull, listed(44, &[])]),
    );
    let found = tracker(&canned)
        .proposed(None, "Say goodbye", "fingerprint abc")
        .unwrap();
    let ids: Vec<&str> = found.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["41", "42"]);
    assert!(tracker(&canned).proposed(None, " ", "x").is_err());
}

/// The working state's tickets are the open issues with its label.
#[test]
fn the_working_state_is_its_label() {
    let mut pull = listed(6, &["agent:working"]);
    pull["pull_request"] = json!({ "merged_at": null });
    let canned = Canned::default().on(
        &format!("GET {ISSUES}?state=open&labels=agent%3Aworking&per_page=100&page=1"),
        200,
        json!([listed(5, &["agent:working"]), pull]),
    );
    let working = tracker(&canned).working(None, "agent:working").unwrap();
    let ids: Vec<&str> = working.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ids, ["5"]);
}

/// Reactions are read for a comment that has some, and for no other.
#[test]
fn reactions_are_read_only_where_counted() {
    let mut reacted = comment(10, "2026-11-12T15:23:14Z", "2026-11-12T15:23:14Z", ann());
    reacted["reactions"] = json!({ "total_count": 1, "+1": 1 });
    let mut quiet = comment(11, "2026-11-12T15:23:20Z", "2026-11-12T15:23:20Z", ann());
    quiet["reactions"] = json!({ "total_count": 0 });
    let canned = Canned::default()
        .on(&format!("GET {ISSUE}"), 200, issue(&[], "open"))
        .on(
            &format!("GET {ISSUE}/comments?per_page=100&page=1"),
            200,
            json!([reacted, quiet]),
        )
        .on(
            "GET repos/demo/project/issues/comments/10/reactions?per_page=100&page=1",
            200,
            json!([{ "content": "+1", "user": { "login": "bea", "id": 2, "type": "User" },
                     "created_at": "2026-11-12T15:23:15Z" }]),
        );
    let comments = tracker(&canned).comments(&id("7")).unwrap();
    assert!(comments[0].reactions[0].is_thumbs_up());
    assert!(matches!(
        comments[0].reactions[0].author,
        Author::Account(_)
    ));
    assert!(comments[1].reactions.is_empty());
    assert_eq!(canned.calls().len(), 3);
}

/// A pass over the ready column reads the list and each ticket's blockers:
/// 1 + n requests (G6 counts events and comments for the tickets the pass
/// takes further). Read again unchanged, each answers 304 and its body is
/// the one kept.
#[test]
fn a_pass_reads_once_and_an_unchanged_one_costs_304s() {
    let canned = Canned::default()
        .on_tagged(
            &format!("GET {READY}"),
            json!([listed(1, &["agent:ready"]), listed(2, &["agent:ready"])]),
            "\"list\"",
        )
        .on_tagged(&blocked_by(1), json!([listed(3, &[])]), "\"b1\"")
        .on_tagged(&blocked_by(2), json!([]), "\"b2\"");
    let tracker = tracker(&canned);
    let first = tracker.admitted(&column(&label())).unwrap();
    assert_eq!(canned.calls().len(), 3);
    let second = tracker.admitted(&column(&label())).unwrap();
    assert_eq!(first, second);
    let again = &canned.calls()[3..];
    assert_eq!(again.len(), 3);
    assert!(again.iter().all(|c| c.ends_with("(304)")), "{again:?}");
}

#[test]
fn a_ticket_is_named_and_closed_by_its_number() {
    let canned = Canned::default();
    let tracker = tracker(&canned);
    assert_eq!(tracker.reference(&id("12")), "#12");
    assert_eq!(tracker.reference(&id("OWL-12")), "OWL-12");
    assert_eq!(
        tracker.closing_line(&id("12")).as_deref(),
        Some("Closes #12")
    );
    assert_eq!(tracker.closing_line(&id("OWL-12")), None);
}

/// A blocker of another repository is never a number, so never one of
/// this repository's tickets, and always a ticket id.
#[test]
fn a_foreign_blocker_is_named_by_its_repository() {
    assert_eq!(
        super::foreign_id("https://api.github.com/repos/Acme/web.site", 12).as_str(),
        "Acme-web_site-12"
    );
    let long = format!("https://api.github.com/repos/{}/x", "a".repeat(80));
    assert_eq!(super::foreign_id(&long, 3).as_str(), "elsewhere-3");
}
