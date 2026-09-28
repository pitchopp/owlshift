//! The GitHub forge adapter on recorded exchanges, the hygiene of those
//! fixtures, and their recorder.
//!
//! The fixtures were recorded read-only on `pitchopp/owlshift`. Opening a
//! pull request is a write, so its answer is made up in the shape GitHub's
//! schema gives it and flagged `synthesized`; the live test
//! (`github_live.rs`) opens a real one.
//!
//! To record again, with a token that can read the repository in
//! `OWLSHIFT_GITHUB_TOKEN`, or logged in with `gh`:
//!
//! ```sh
//! cargo test -p owlshift-adapters --test github -- --ignored
//! ```

mod support;

use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};

use owlshift_adapters::forge::github::{GitHubForge, HttpTransport, NewPullRequest, Token};
use owlshift_adapters::forge::{
    Branch, CheckKind, CheckState, CommitId, ErrorKind, PrState, Repo, Verdict,
};

use support::{Recorder, Replay};

const CHECKS: &str = "checks-pr-16-paged.json";
const NO_OPEN_PR: &str = "no-open-pull-request.json";
const OPEN: &str = "open-pull-request.json";
const UNKNOWN_PR: &str = "unknown-pull-request.json";
const UNAUTHORIZED: &str = "unauthorized.json";

/// PR #16 (OWL-13), merged, with four GitHub Actions check runs.
const PR_16: u64 = 16;
const PR_16_HEAD: &str = "36f0557116ea73f9b0805980e83b360e23b99940";

fn dir() -> PathBuf {
    support::fixtures("github")
}

fn repo() -> Repo {
    Repo::parse("pitchopp/owlshift").unwrap()
}

fn forge(replay: &Replay) -> GitHubForge {
    GitHubForge::with_transport(replay.clone(), repo())
}

fn head() -> CommitId {
    CommitId::new(PR_16_HEAD).unwrap()
}

fn no_such_branch() -> Branch {
    Branch::new("owlshift-fixture-no-such-branch").unwrap()
}

fn main_branch() -> Branch {
    Branch::new("main").unwrap()
}

const BODY: &str = "Runner-written body.\n\n- one\n- two\n";

fn new_pull_request<'a>(head: &'a Branch, base: &'a Branch) -> NewPullRequest<'a> {
    NewPullRequest {
        head,
        base,
        title: "Fixture pull request",
        body: BODY,
        draft: false,
    }
}

/// Recorded one check per page: four pages, every one read.
#[test]
fn reads_every_check_of_a_pull_request_across_pages() {
    let replay = Replay::of(&dir(), CHECKS);
    let set = forge(&replay).with_check_page(1).checks(PR_16, &head()).unwrap();
    replay.assert_done();
    assert_eq!(set.head, head());
    assert_eq!(set.state, PrState::Merged);
    assert_eq!(set.verdict(), Verdict::NotOpen);
    let mut names: Vec<_> = set.checks.iter().map(|c| c.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "check (macos-latest)",
            "check (ubuntu-latest)",
            "check (windows-latest)",
            "ci"
        ]
    );
    for check in &set.checks {
        assert_eq!(check.state, CheckState::Passed, "{check:?}");
        assert_eq!(check.raw, "SUCCESS");
        assert!(!check.required, "the repository has no branch protection");
        let CheckKind::CheckRun { app, .. } = &check.kind else {
            panic!("{check:?}");
        };
        assert_eq!(app.as_deref(), Some("github-actions"));
    }
}

#[test]
fn finds_no_open_pull_request_for_a_branch_without_one() {
    let replay = Replay::of(&dir(), NO_OPEN_PR);
    let found = forge(&replay)
        .find_open_pull_request(&no_such_branch(), &main_branch())
        .unwrap();
    replay.assert_done();
    assert_eq!(found, None);
}

#[test]
fn opens_a_pull_request_with_the_runners_body_verbatim() {
    let replay = Replay::of(&dir(), OPEN);
    let (head, base) = (no_such_branch(), main_branch());
    let pr = forge(&replay)
        .open_pull_request(new_pull_request(&head, &base))
        .unwrap();
    // The replay checked the request, body included, against the recording.
    replay.assert_done();
    assert_eq!(pr.state, PrState::Open);
    assert!(pr.url.ends_with(&format!("/pull/{}", pr.number)), "{}", pr.url);
    let fixture = support::load(&dir(), OPEN);
    assert_eq!(
        fixture.exchanges[1].request["variables"]["input"]["body"],
        BODY
    );
}

#[test]
fn an_unknown_pull_request_is_not_found() {
    let replay = Replay::of(&dir(), UNKNOWN_PR);
    let error = forge(&replay).checks(999_999, &head()).unwrap_err();
    replay.assert_done();
    assert_eq!(error.kind, ErrorKind::NotFound, "{error}");
}

#[test]
fn a_rejected_token_is_unauthorized() {
    let replay = Replay::of(&dir(), UNAUTHORIZED);
    let error = forge(&replay).checks(PR_16, &head()).unwrap_err();
    replay.assert_done();
    assert_eq!(error.kind, ErrorKind::Unauthorized, "{error}");
}

/// No fixture may hold a token, a header or an e-mail address, and only the
/// write is made up.
#[test]
fn fixtures_hold_no_credential_and_flag_what_is_made_up() {
    for entry in fs::read_dir(dir()).unwrap() {
        let path = entry.unwrap().path();
        let text = fs::read_to_string(&path).unwrap();
        for forbidden in ["ghp_", "gho_", "ghu_", "github_pat_", "uthorization", "@"] {
            assert!(
                !text.contains(forbidden),
                "{} contains {forbidden:?}",
                path.display()
            );
        }
        let fixture: support::Fixture = serde_json::from_str(&text).unwrap();
        for exchange in &fixture.exchanges {
            let query = exchange.request["query"].as_str().unwrap_or_default();
            assert_eq!(
                query.starts_with("mutation"),
                exchange.synthesized.is_some(),
                "{}: a write must be made up, a read recorded",
                path.display()
            );
        }
    }
}

/// The answer GitHub gives `createPullRequest`, in its schema's shape, for a
/// request that is never sent.
fn made_up_pull_request(request: &Value) -> Option<(Value, String)> {
    let query = request["query"].as_str()?;
    query.starts_with("mutation").then(|| {
        let answer = json!({ "data": { "createPullRequest": { "pullRequest": {
            "number": 9999,
            "url": "https://github.com/pitchopp/owlshift/pull/9999",
            "state": "OPEN",
            "headRefOid": "0000000000000000000000000000000000000000",
        } } } });
        (
            answer,
            "not sent: a write, answered in the shape of GitHub's schema".to_owned(),
        )
    })
}

/// Records the fixtures above against the live API. Read only: the one
/// write is made up, never sent.
#[test]
#[ignore = "records against the live GitHub API"]
fn record_fixtures() {
    let token = support::github_token();
    let recorded = format!(
        "{} on pitchopp/owlshift, recorded live",
        jiff::Zoned::now().date()
    );
    let recorder = || Recorder::over(HttpTransport::new(token.clone()), dir());

    let r = recorder();
    GitHubForge::with_transport(r.clone(), repo())
        .with_check_page(1)
        .checks(PR_16, &head())
        .unwrap();
    r.write(CHECKS, &recorded);

    let r = recorder();
    let found = GitHubForge::with_transport(r.clone(), repo())
        .find_open_pull_request(&no_such_branch(), &main_branch())
        .unwrap();
    assert_eq!(found, None);
    r.write(NO_OPEN_PR, &recorded);

    let r = recorder().making_up(made_up_pull_request);
    let (h, b) = (no_such_branch(), main_branch());
    GitHubForge::with_transport(r.clone(), repo())
        .open_pull_request(new_pull_request(&h, &b))
        .unwrap();
    r.write(OPEN, &recorded);

    let r = recorder();
    let error = GitHubForge::with_transport(r.clone(), repo())
        .checks(999_999, &head())
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::NotFound, "{error}");
    r.write(UNKNOWN_PR, &recorded);

    // A well-formed token that GitHub does not know.
    let rejected = Token::new(format!("ghp_{}", "0".repeat(36)));
    let r = Recorder::over(HttpTransport::new(rejected), dir());
    let error = GitHubForge::with_transport(r.clone(), repo())
        .checks(PR_16, &head())
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unauthorized, "{error}");
    r.write(UNAUTHORIZED, &recorded);
}
