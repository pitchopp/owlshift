//! OWL-535: the GitHub Issues tracker against the live API, writes included,
//! on the private repository `pitchopp/owlshift-live-sandbox`. By hand only:
//! it never runs in CI. The token comes from `OWLSHIFT_GITHUB_TOKEN`, or the
//! `gh` login, never from the command line, and is never printed; it must
//! read and write the sandbox's issues (the GitHub App's user token, once its
//! installation accepted Issues, as for live check G1).
//!
//! ```sh
//! cargo test -p owlshift-adapters --test github_tracker_live -- --ignored --nocapture
//! ```
//!
//! It opens an issue, reads it, comments on it twice, moves its stage there
//! and back and reads it, then closes it as not planned, whatever happened.

mod support;

use serde_json::{Value, json};

use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github::{GitHubForge, HttpTransport, Token};
use owlshift_adapters::graphql::Transport;
use owlshift_adapters::tracker::github::GitHubTracker;
use owlshift_adapters::tracker::{Author, ErrorKind, Tracker};
use owlshift_contracts::config;
use owlshift_contracts::ids::TicketId;

const SANDBOX: &str = "pitchopp/owlshift-live-sandbox";
const THERE: &str = "agent:needs-input";
const BACK: &str = "agent:in-progress";

fn tracker_config() -> config::Tracker {
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

/// Closes the issue as not planned when dropped, through GraphQL (REST
/// closes with a PATCH, which no transport sends), and says whether GitHub
/// took it.
struct Close {
    token: Token,
    number: u64,
    node: String,
}

impl Drop for Close {
    fn drop(&mut self) {
        let answer = HttpTransport::new(self.token.clone()).send(&json!({
            "query": "mutation Close($id: ID!) { closeIssue(input: { issueId: $id, \
                      stateReason: NOT_PLANNED }) { issue { state } } }",
            "variables": { "id": self.node },
        }));
        match answer {
            Ok(answer) if answer.body.contains("\"CLOSED\"") => {
                eprintln!("#{} is closed", self.number)
            }
            other => eprintln!(
                "CLOSE FAILED: #{} may still be open: {other:?}",
                self.number
            ),
        }
    }
}

#[test]
#[ignore = "writes to the live GitHub API on pitchopp/owlshift-live-sandbox"]
fn the_github_tracker_reads_comments_and_moves_the_stage_live() {
    let token = support::github_token();
    let opened = HttpTransport::new(token.clone())
        .post(
            &format!("repos/{SANDBOX}/issues"),
            &json!({
                "title": "Owlshift live test: the GitHub Issues tracker",
                "body": "Opened by `github_tracker_live.rs` (OWL-535), closed by it.",
            }),
        )
        .unwrap();
    assert_eq!(opened.status, 201, "{}", opened.body);
    let opened: Value = serde_json::from_str(&opened.body).unwrap();
    let number = opened["number"].as_u64().unwrap();
    let _close = Close {
        token: token.clone(),
        number,
        node: opened["node_id"].as_str().unwrap().to_owned(),
    };
    eprintln!("opened #{number}");

    let forge = GitHubForge::new(token, Repo::parse(SANDBOX).unwrap());
    let tracker = GitHubTracker::new(forge, &tracker_config());
    let id = TicketId::new(number.to_string()).unwrap();

    let ticket = tracker.ticket(&id).unwrap();
    assert_eq!(
        ticket.title,
        "Owlshift live test: the GitHub Issues tracker"
    );
    assert!(matches!(ticket.author, Author::Other { .. }));
    assert_eq!(tracker.stage(&id).unwrap(), "open");

    // Two comments at once share a second, and read in the order posted.
    let first = tracker
        .post_comment(&id, "First, from the live test.")
        .unwrap();
    let second = tracker
        .post_comment(&id, "Second, from the live test.")
        .unwrap();
    assert!(matches!(first.author, Author::Account(_)), "{first:?}");
    let comments = tracker.comments(&id).unwrap();
    assert_eq!(comments, [first, second]);
    assert!(comments[0].created_at < comments[1].created_at);

    tracker.set_stage(&id, THERE).unwrap();
    assert_eq!(tracker.stage(&id).unwrap(), THERE);
    tracker.set_stage(&id, BACK).unwrap();
    assert_eq!(tracker.stage(&id).unwrap(), BACK);
    let labels = tracker.ticket(&id).unwrap().labels;
    assert_eq!(labels, [BACK], "the other stage label was removed");

    let pull = tracker.ticket(&TicketId::new("18").unwrap()).unwrap_err();
    assert_eq!(pull.kind, ErrorKind::NotFound, "{pull}");
}
