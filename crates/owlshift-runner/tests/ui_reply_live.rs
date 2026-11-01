//! Live check U2 of the build plan, "P8 · Local web UI" (OWL-466): a reply
//! the local web UI posts through the operator's Linear key, its last line
//! `go`, is the decider's own and counts at once.
//!
//! On a real ticket assigned to the key's own account, it reads the key's
//! account (`viewer`) and the ticket's decider as the runner resolves it (the
//! assignee), posts one comment through the runner's Linear opener, reads the
//! ticket's comments back, and judges them with the runner's own functions
//! against an ask taken as posted one second, by Linear's clock, before the
//! comment: the comment's author is the key's account (`user.app` false), the
//! runner names it the decider (`on_demand::account_author`), it is a new
//! answer (`answer_check::new_answer`), and with the default quiet window it
//! counts at once (`answer_check::readiness`), while the same text without
//! `go` would still settle. The ask is the test's own, not one an agent run
//! posted: the loop of `owlshift do` and `owlshift continue` is not run.
//!
//! It posts a comment, which stays, so it never runs in CI and names its
//! ticket:
//!
//! ```sh
//! OWLSHIFT_LIVE_LINEAR=<ticket> LINEAR_ENV_FILE=<dotenv file with LINEAR_API_KEY=> \
//!   cargo test -p owlshift-runner --test ui_reply_live -- --ignored --nocapture
//! ```
//!
//! The key comes from `LINEAR_API_KEY`, or from the dotenv file
//! `LINEAR_ENV_FILE` names; it goes through an in-memory keychain, as
//! `writer_live` does, and is never printed. The key's workspace must be
//! `OWLSHIFT_LIVE_WORKSPACE` (`owlshift` by default), and the ticket's
//! assignee the key's account: otherwise it stops before posting. It prints
//! the posted comment's id and yes or no for each check, never an account's
//! id or name.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_adapters::graphql::Transport;
use owlshift_adapters::tracker::linear::{ApiKey, HttpTransport};
use owlshift_adapters::tracker::{Author as TrackerAuthor, Comment, Person, Tracker};
use owlshift_contracts::brief::Relation;
use owlshift_contracts::ids::{QuestionId, TicketId};
use owlshift_contracts::refs::{Ask, AskDecider, AskKind, TicketQuestions};
use owlshift_contracts::result::{Question, QuestionOption, Recommendation};
use owlshift_core::decider::{self, ZoneOwners};
use owlshift_core::reply::DEFAULT_QUIET_WINDOW;
use owlshift_platform::keychain::{Keychain, Secret};
use owlshift_runner::answer_check::{Readiness, new_answer, readiness};
use owlshift_runner::on_demand::account_author;
use owlshift_runner::secrets;
use owlshift_runner::tracker::{LINEAR_ACCOUNT, linear};

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("this test writes to Linear: set {name}"))
}

fn linear_key() -> String {
    if let Ok(key) = std::env::var("LINEAR_API_KEY") {
        return key.trim().to_owned();
    }
    let file = required("LINEAR_ENV_FILE");
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{file}: {e}"));
    text.lines()
        .find_map(|line| line.strip_prefix("LINEAR_API_KEY="))
        .unwrap_or_else(|| panic!("{file} has no LINEAR_API_KEY= line"))
        .trim()
        .to_owned()
}

/// The key's own account and its workspace, read as the UI reads them once
/// at its start.
struct Viewer {
    id: String,
    app: bool,
    workspace: String,
}

fn viewer(key: &str) -> Viewer {
    let answer = HttpTransport::new(ApiKey::new(key))
        .send(&serde_json::json!({
            "query": "{ viewer { id app } organization { urlKey } }",
            "variables": {},
        }))
        .expect("an answer from Linear");
    assert_eq!(answer.status, 200, "Linear refused the key's viewer query");
    let body: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    let data = &body["data"];
    Viewer {
        id: data["viewer"]["id"].as_str().expect("viewer.id").to_owned(),
        app: data["viewer"]["app"].as_bool().expect("viewer.app"),
        workspace: data["organization"]["urlKey"]
            .as_str()
            .expect("organization.urlKey")
            .to_owned(),
    }
}

fn question() -> Question {
    Question {
        id: QuestionId::new("Q1").unwrap(),
        category: "scope".into(),
        context: String::new(),
        text: "Live check U2: does a reply through the operator's key count?".into(),
        options: vec![QuestionOption::new("Yes"), QuestionOption::new("No")],
        recommendation: Some(Recommendation {
            option: Some("A".into()),
            why: "Synthetic ask of the live check.".into(),
        }),
        replan: false,
    }
}

fn yes(ok: bool) -> &'static str {
    if ok { "yes" } else { "NO" }
}

#[test]
#[ignore = "live: posts a comment on Linear; set OWLSHIFT_LIVE_LINEAR=<ticket>"]
fn a_reply_through_the_operators_key_is_the_deciders_and_counts_at_once() {
    let ticket = TicketId::new(required("OWLSHIFT_LIVE_LINEAR")).unwrap();
    let workspace =
        std::env::var("OWLSHIFT_LIVE_WORKSPACE").unwrap_or_else(|_| "owlshift".to_owned());
    let key = linear_key();
    println!("key: present");
    let me = viewer(&key);
    assert_eq!(me.workspace, workspace, "the key is of another workspace");
    assert!(
        !me.app,
        "the key's account is an app: not an operator's key"
    );

    // The runner's own opener, on the shared account as `owlshift init`
    // stores it when no project account is set; no Linear app is stored, so
    // the comment goes through the key.
    let keychain = Arc::new(Keychain::in_memory());
    keychain.store(LINEAR_ACCOUNT, &Secret::new(key)).unwrap();
    let repo = Repo::parse("pitchopp/owlshift").unwrap();
    let mut look = |account: &str| keychain.read(account).map_err(|e| e.to_string());
    let team = ticket.as_str().rsplit_once('-').unwrap().0;
    let tracker = linear(&secrets::tracker(&repo, &mut look).unwrap(), team).unwrap();

    // The decider as the runner resolves it for an ask: the assignee.
    let read = tracker.ticket(&ticket).unwrap();
    let assignee = read.assignee.as_ref().map(|person| person.id.as_str());
    let owners = ZoneOwners::default();
    let found = decider::decider(assignee, &[], &owners).expect("the ticket has an assignee");
    let ask_decider = AskDecider {
        account: found.account().to_owned(),
        by: found.rule(),
    };
    assert_eq!(
        ask_decider.account, me.id,
        "the ticket's assignee is not the key's account: assign it to them first"
    );
    let decider = Person {
        id: ask_decider.account.clone(),
        name: read.assignee.as_ref().unwrap().name.clone(),
    };

    let body = "Live check U2 of the local web UI (OWL-466, the `ui_reply_live` test): a reply \
                posted through the operator's Linear key, as the UI will post one. It answers \
                no question of Owlshift's.\n\ngo";
    let posted = tracker.post_comment(&ticket, body).unwrap();
    println!("posted comment {} on {ticket}", posted.id);

    let comments = tracker.comments(&ticket).unwrap();
    let comment: &Comment = comments
        .iter()
        .find(|comment| comment.id == posted.id)
        .expect("the posted comment is read back");
    // The ask, a second before the comment by Linear's own clock.
    let mut asked = TicketQuestions::new();
    asked.asks.push(Ask {
        kind: AskKind::Questions,
        round: NonZeroU32::new(1).unwrap(),
        at: comment.created_at - Duration::from_secs(1),
        comment: "u2-synthetic-ask".into(),
        questions: vec![question()],
        decider: ask_decider.clone(),
        verdicts: Vec::new(),
        result_refusal: None,
    });

    let author_is_key = matches!(&comment.author, TrackerAuthor::Account(p) if p.id == me.id);
    let named_decider = account_author(&comment.author, &decider).relation == Relation::Decider;
    let answer = new_answer(&comments, &decider, &asked).is_some();
    let counts = readiness(
        &comments,
        &decider,
        &asked,
        Timestamp::now(),
        DEFAULT_QUIET_WINDOW,
    ) == Some(Readiness::Counts);
    // Control: the same comment without its `go` line, judged at its own
    // time, still settles through the quiet window.
    let without_go: Vec<Comment> = comments
        .iter()
        .map(|c| {
            let mut c = c.clone();
            if c.id == posted.id {
                c.body = c.body.trim_end().trim_end_matches("go").to_owned();
            }
            c
        })
        .collect();
    let settles = matches!(
        readiness(
            &without_go,
            &decider,
            &asked,
            comment.created_at,
            DEFAULT_QUIET_WINDOW
        ),
        Some(Readiness::Settling { .. })
    );
    println!(
        "key account app=false: yes; author is the key's account (Account, app false): {}; \
         runner names it the decider: {}; new answer: {}; counts at once with `go`: {}; \
         without `go` it settles: {}",
        yes(author_is_key),
        yes(named_decider),
        yes(answer),
        yes(counts),
        yes(settles)
    );
    assert!(author_is_key && named_decider && answer && counts && settles);
}
