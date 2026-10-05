//! OWL-157's live acceptance: a question round posted through the Linear app
//! user reaches its decider. It writes to Linear, so it never runs in CI; the
//! maintainer runs it by hand:
//!
//! ```sh
//! LINEAR_ENV_FILE=$HOME/Projects/owlshift/.env \
//!   cargo test -p owlshift-runner --test linear_app_live -- --ignored --nocapture
//! ```
//!
//! It reads `LINEAR_API_KEY`, `LINEAR_APP_CLIENT_ID` and
//! `LINEAR_APP_CLIENT_SECRET` from the dotenv file `LINEAR_ENV_FILE` names,
//! never from the environment or the command line, and prints outcomes only:
//! never a key, a secret, a token or a person's name. On the Owlshift
//! workspace (checked first), team `OWL`, through the runner's own code:
//!
//! 1. creates a test issue through the key, assigned to the key's account,
//!    the decider of this check;
//! 2. opens the tracker as `owlshift do` does (`owlshift_runner::tracker::
//!    linear`): the app's token is requested and checked;
//! 3. posts a QUESTIONS comment through the Writer, mentioning the decider,
//!    and reads it back through the tracker;
//! 4. moves the issue's stage through the tracker;
//! 5. requests two more tokens with the same scopes, revokes one, and checks
//!    the other still answers;
//! 6. waits 15 s, then reads the decider's notifications on the issue.
//!
//! Whatever happens, the comment is deleted, the issue goes to Linear's
//! trash, and the tracker's token is revoked when it is dropped.

use std::num::NonZeroU32;
use std::time::Duration;

use serde_json::{Value, json};

use owlshift_adapters::tracker::linear::app::{ClientCredentials, HttpOAuth, OAuth};
use owlshift_adapters::tracker::linear::{ApiKey, HttpTransport, Transport};
use owlshift_adapters::tracker::{Author, Tracker};
use owlshift_contracts::comment::{MarkedComment, MarkerKind};
use owlshift_contracts::ids::{QuestionId, TicketId};
use owlshift_contracts::result::Question;
use owlshift_platform::keychain::{Keychain, Secret};
use owlshift_runner::tracker::{
    LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT, LINEAR_APP_SECRET_ACCOUNT, linear,
};
use owlshift_runner::writer::{QuestionsComment, Writer};

const TEAM: &str = "OWL";

/// The three values, from the dotenv file only.
struct Env {
    key: String,
    id: String,
    secret: String,
}

fn env() -> Env {
    let file = std::env::var("LINEAR_ENV_FILE")
        .expect("this test writes to Linear: set LINEAR_ENV_FILE to the dotenv file");
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{file}: {e}"));
    let value = |name: &str| {
        let found = text.lines().find_map(|line| {
            line.trim()
                .strip_prefix(name)
                .and_then(|rest| rest.strip_prefix('='))
                .map(|value| value.trim().trim_matches(['"', '\'']).to_owned())
        });
        println!(
            "env {name}: {}",
            if found.is_some() {
                "present"
            } else {
                "MISSING"
            }
        );
        found.unwrap_or_default()
    };
    let env = Env {
        key: value("LINEAR_API_KEY"),
        id: value("LINEAR_APP_CLIENT_ID"),
        secret: value("LINEAR_APP_CLIENT_SECRET"),
    };
    assert!(
        !env.key.is_empty() && !env.id.is_empty() && !env.secret.is_empty(),
        "a value is missing: nothing was called"
    );
    env
}

/// One GraphQL request through the key; its `data`, or a panic naming
/// Linear's error code.
fn query(key: &ApiKey, query: &str, variables: Value) -> Value {
    let response = HttpTransport::new(key.clone())
        .send(&json!({ "query": query, "variables": variables }))
        .unwrap();
    let answer: Value = serde_json::from_str(&response.body).unwrap();
    if let Some(error) = answer["errors"].get(0) {
        panic!(
            "HTTP {}: {} {}",
            response.status, error["extensions"]["code"], error["message"]
        );
    }
    answer["data"].clone()
}

/// Deletes the comment and trashes the issue when dropped, and says whether
/// Linear confirms each.
struct Cleanup {
    key: ApiKey,
    issue: String,
    comment: Option<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let mutate = |text: &str, id: &str| {
            let response = HttpTransport::new(self.key.clone())
                .send(&json!({ "query": text, "variables": { "id": id } }))
                .map(|r| r.body)
                .unwrap_or_default();
            response.contains("\"success\":true")
        };
        if let Some(comment) = &self.comment {
            let done = mutate(
                "mutation D($id: String!) { commentDelete(id: $id) { success } }",
                comment,
            );
            println!("cleanup commentDelete: {done}");
        }
        let done = mutate(
            "mutation D($id: String!) { issueDelete(id: $id) { success } }",
            &self.issue,
        );
        println!("cleanup issueDelete (trash): {done}");
    }
}

#[test]
#[ignore = "live: writes to Linear through the app; set LINEAR_ENV_FILE"]
fn a_question_round_through_the_app_reaches_its_decider() {
    let env = env();
    let key = ApiKey::new(env.key.clone());
    let workspace = query(&key, "{ organization { urlKey } }", json!({}));
    assert_eq!(
        workspace["organization"]["urlKey"], "owlshift",
        "the Owlshift workspace only"
    );
    let viewer = query(&key, "{ viewer { id } }", json!({}));
    let decider = viewer["viewer"]["id"].as_str().unwrap().to_owned();
    let teams = query(
        &key,
        "query T($key: String!) { teams(filter: { key: { eq: $key } }) { nodes { id } } }",
        json!({ "key": TEAM }),
    );
    let team = teams["teams"]["nodes"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 1. The test issue, through the key.
    let created = query(
        &key,
        "mutation C($input: IssueCreateInput!) { issueCreate(input: $input) { success \
         issue { id identifier } } }",
        json!({ "input": {
            "teamId": team,
            "assigneeId": decider,
            "title": "[owlshift] OWL-157 live check: a question round through the Linear app \
                      user (deleted by the check)",
            "description": "Created by `linear_app_live`. Safe to ignore: the check deletes it.",
        } }),
    );
    let issue = &created["issueCreate"]["issue"];
    let mut cleanup = Cleanup {
        key: key.clone(),
        issue: issue["id"].as_str().unwrap().to_owned(),
        comment: None,
    };
    let ticket = TicketId::new(issue["identifier"].as_str().unwrap()).unwrap();
    println!("test issue: {ticket}");

    // 2. The tracker as `owlshift do` opens it.
    let keychain = Keychain::in_memory();
    for (account, value) in [
        (LINEAR_ACCOUNT, &env.key),
        (LINEAR_APP_ID_ACCOUNT, &env.id),
        (LINEAR_APP_SECRET_ACCOUNT, &env.secret),
    ] {
        keychain
            .store(account, &Secret::new(value.as_str()))
            .unwrap();
    }
    let tracker = linear(&keychain, TEAM).unwrap_or_else(|e| panic!("{e}"));
    println!("tracker opened with the app: ok");

    // 3. A QUESTIONS round through the Writer, read back.
    let round = QuestionsComment {
        ticket: ticket.clone(),
        round: NonZeroU32::new(1).unwrap(),
        summary: "OWL-157 live check: a question round as Owlshift posts it through its Linear \
                  app user. Safe to ignore: the check deletes it."
            .to_owned(),
        questions: vec![Question {
            id: QuestionId::new("Q1").unwrap(),
            category: "scope".to_owned(),
            context: "Nothing to decide: this checks how Linear tells you of a question."
                .to_owned(),
            text: "Did Linear notify you of this comment?".to_owned(),
            options: Vec::new(),
            recommendation: None,
        }],
        premise_false: false,
        decided: 0,
        fallback: None,
    };
    let posted = Writer::new(&tracker)
        .mentioning(&decider)
        .post_questions(&round)
        .unwrap();
    cleanup.comment = Some(posted.id.clone());
    let read = tracker.comments(&ticket).unwrap();
    let back = read.iter().find(|c| c.id == posted.id).expect("read back");
    println!(
        "comment author is not an account: {}",
        matches!(back.author, Author::Other { .. })
    );
    println!(
        "comment body read back verbatim: {}",
        back.body == posted.body
    );
    let marked = MarkedComment::parse(&back.body).unwrap().unwrap();
    println!(
        "comment marked QUESTIONS: {}",
        marked.header.kind == MarkerKind::Questions
    );
    let mentions = back
        .body
        .contains("\n\nWaiting for https://linear.app/owlshift/profiles/");
    println!("comment mentions the decider's profile link: {mentions}");
    let author = query(
        &key,
        "query A($id: String!) { comment(id: $id) { user { id app } } }",
        json!({ "id": posted.id }),
    );
    let app_user = author["comment"]["user"]["id"].as_str().unwrap().to_owned();
    println!("comment user.app: {}", author["comment"]["user"]["app"]);

    // 4. A stage move through the tracker.
    tracker.set_stage(&ticket, "Backlog").unwrap();
    println!("stage moved through the app: ok");

    // 5. Two more tokens with the same scopes: revoking one leaves the other.
    let oauth = HttpOAuth::new(ClientCredentials::new(env.id.as_str(), env.secret.as_str()));
    let first = oauth.token().unwrap();
    let second = oauth.token().unwrap();
    oauth.revoke(&second).unwrap();
    let ask = json!({ "query": "{ viewer { id } }" });
    let kept = oauth.send(&first, &ask).unwrap().status;
    let gone = oauth.send(&second, &ask).unwrap().status;
    println!("after revoking a second token: the first answers HTTP {kept}, the second {gone}");
    let revoked = oauth.revoke(&first);
    println!("first token revoked: {}", revoked.is_ok());

    // 6. The decider's notifications on the issue.
    std::thread::sleep(Duration::from_secs(15));
    let notes = query(
        &key,
        "{ notifications(first: 50) { nodes { type actor { id } \
         ... on IssueNotification { issue { id } comment { id } } } } }",
        json!({}),
    );
    let on_issue: Vec<&Value> = notes["notifications"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| n["issue"]["id"] == cleanup.issue.as_str())
        .collect();
    for note in &on_issue {
        println!(
            "notification: type {}, actor is the app user {}, on the comment {}",
            note["type"],
            note["actor"]["id"] == app_user.as_str(),
            note["comment"]["id"] == posted.id.as_str()
        );
    }
    let mentioned = on_issue.iter().any(|n| {
        n["type"] == "issueCommentMention"
            && n["actor"]["id"] == app_user.as_str()
            && n["comment"]["id"] == posted.id.as_str()
    });

    assert!(matches!(back.author, Author::Other { .. }));
    assert_eq!(back.body, posted.body, "Linear did not keep the body");
    assert_eq!(marked.header.kind, MarkerKind::Questions);
    assert!(mentions, "no mention line");
    assert_eq!(author["comment"]["user"]["app"], true);
    assert_eq!((kept, gone), (200, 401));
    assert!(mentioned, "no issueCommentMention from the app user");
    drop(tracker);
    println!("tracker dropped: its token is revoked");
}
