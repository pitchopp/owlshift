//! The Writer end to end on real services: it reads the check set of a real
//! open pull request on GitHub, then posts the delivery report on a real
//! Linear ticket. It writes a comment, so it never runs in CI and names its
//! ticket explicitly:
//!
//! ```sh
//! OWLSHIFT_LIVE_LINEAR=<ticket> \
//! OWLSHIFT_LIVE_GITHUB=<owner>/<repo> OWLSHIFT_LIVE_BRANCH=<branch> \
//! LINEAR_ENV_FILE=<dotenv file with LINEAR_API_KEY=> \
//!   cargo test -p owlshift-runner --test writer_live -- --ignored --nocapture
//! ```
//!
//! Post only on a ticket of the Owlshift workspace that the report is about,
//! or a test ticket: the key's workspace is not checked here. The Linear key
//! comes from `LINEAR_API_KEY`, or from the dotenv file `LINEAR_ENV_FILE`
//! names; the GitHub token from `OWLSHIFT_GITHUB_TOKEN`, or from the `gh`
//! login. Both go through an in-memory keychain and the runner's own openers,
//! as `owlshift do` will use them. Neither is ever printed.

use std::process::Command;

use owlshift_adapters::forge::{Branch, Repo};
use owlshift_adapters::tracker::Tracker;
use owlshift_contracts::comment::{MarkedComment, MarkerKind};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::result::Decision;
use owlshift_platform::keychain::{Keychain, Secret};
use owlshift_runner::forge::{GITHUB_ACCOUNT, github};
use owlshift_runner::tracker::{LINEAR_ACCOUNT, linear};
use owlshift_runner::writer::{DeliveryReport, Gate, Writer};

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("this test writes to Linear: set {name}"))
}

fn linear_key() -> Secret {
    if let Ok(key) = std::env::var("LINEAR_API_KEY") {
        return Secret::new(key);
    }
    let file = required("LINEAR_ENV_FILE");
    let text = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{file}: {e}"));
    let key = text
        .lines()
        .find_map(|line| line.strip_prefix("LINEAR_API_KEY="))
        .unwrap_or_else(|| panic!("{file} has no LINEAR_API_KEY= line"));
    Secret::new(key.trim())
}

fn github_token() -> Secret {
    if let Ok(token) = std::env::var("OWLSHIFT_GITHUB_TOKEN") {
        return Secret::new(token);
    }
    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .expect("a GitHub token in OWLSHIFT_GITHUB_TOKEN, or a logged-in `gh`");
    assert!(output.status.success(), "`gh auth token` failed");
    Secret::new(String::from_utf8(output.stdout).unwrap().trim())
}

#[test]
#[ignore = "live: posts a comment on Linear; set OWLSHIFT_LIVE_LINEAR=<ticket>"]
fn posts_the_delivery_report_on_a_real_ticket() {
    let ticket = TicketId::new(required("OWLSHIFT_LIVE_LINEAR")).unwrap();
    let repo = Repo::parse(&required("OWLSHIFT_LIVE_GITHUB")).unwrap();
    let branch = Branch::new(required("OWLSHIFT_LIVE_BRANCH")).unwrap();

    let keychain = Keychain::in_memory();
    keychain.store(LINEAR_ACCOUNT, &linear_key()).unwrap();
    keychain.store(GITHUB_ACCOUNT, &github_token()).unwrap();
    let tracker = linear(&keychain).unwrap();
    let forge = github(&keychain, repo).unwrap();

    assert_eq!(tracker.ticket(&ticket).unwrap().id, ticket);
    let pull_request = forge
        .find_open_pull_request(&branch, &Branch::new("main").unwrap())
        .unwrap()
        .expect("an open pull request from the branch into main");
    let checks = forge.checks(pull_request.number, &pull_request.head);

    let report = DeliveryReport {
        ticket: ticket.clone(),
        summary: "Live check of the Writer (the writer_live test), run by hand: a real \
                  delivery report for this pull request, not posted by an agent run."
            .to_owned(),
        pull_request,
        checks,
        gate: Gate::NotRecorded,
        decisions: vec![Decision {
            question: "How this report was posted".to_owned(),
            decision: "By the Writer directly, since `owlshift do` (OWL-20) is not built yet"
                .to_owned(),
            basis: "The ticket's acceptance, validated before OWL-20".to_owned(),
        }],
        followups: vec![],
    };
    let writer = Writer::new(&tracker);
    let posted = writer.post_delivery_report(&report).unwrap();
    println!("posted comment {} on {ticket}", posted.id);

    assert_eq!(
        posted.body,
        report.render(),
        "Linear did not store the body verbatim"
    );
    let marked = MarkedComment::parse(&posted.body).unwrap().unwrap();
    assert_eq!(marked.header.kind, MarkerKind::Delivery);
    assert_eq!(marked.footer.expect("the footer was kept").ticket, ticket);

    // Read back from the ticket, then retried: the same report is not
    // posted twice.
    let count = tracker.comments(&ticket).unwrap().len();
    let again = writer.post_delivery_report(&report).unwrap();
    assert_eq!(again.id, posted.id);
    assert_eq!(tracker.comments(&ticket).unwrap().len(), count);
}
