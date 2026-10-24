//! The GitHub forge adapter on recorded exchanges, the hygiene of those
//! fixtures, and their recorder.
//!
//! The fixtures were recorded read-only on `pitchopp/owlshift`. Opening a
//! pull request is a write, so its answer is made up in the shape GitHub's
//! schema gives it and flagged `synthesized`; the live test
//! (`github_live.rs`) opens a real one.
//!
//! Watch's reads (OWL-209) were recorded live on 2026-10-12 (OWL-217):
//! `follow-merged.json` and `base-checks.json` read-only here, by
//! `record_watch_fixtures`; `follow-red.json` and `follow-conflicting.json`
//! on a disposable repository, by `github_live.rs`'s `records_watch_reads`.
//! `rate-limited.json` alone is made up in the shape GitHub documents, and
//! says so and why in its `recorded` line and on each exchange.
//!
//! The reviews' read (OWL-307) was recorded live on 2026-10-16 by
//! `record_review_fixtures`, read-only, on throwaway pull request #2 of the
//! private repository `pitchopp/owlshift-live-sandbox`, whose reviews were
//! made for it: a review with a body and an inline comment a later push made
//! outdated, one with an inline comment alone, and a reply in a thread.
//!
//! To record again, with a token that can read the repository in
//! `OWLSHIFT_GITHUB_TOKEN`, or logged in with `gh` (`record_fixtures` for
//! delivery's fixtures, `record_watch_fixtures` for watch's):
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
    Branch, CheckKind, CheckState, CommitId, ErrorKind, MergeAnswer, MergeMethod, Mergeable,
    PrState, Repo, Verdict,
};

use support::{Recorder, Replay};

const CHECKS: &str = "checks-pr-16-paged.json";
const NO_OPEN_PR: &str = "no-open-pull-request.json";
const OPEN: &str = "open-pull-request.json";
const UNKNOWN_PR: &str = "unknown-pull-request.json";
const UNAUTHORIZED: &str = "unauthorized.json";
const FOLLOW_RED: &str = "follow-red.json";
const FOLLOW_CONFLICTING: &str = "follow-conflicting.json";
const FOLLOW_MERGED: &str = "follow-merged.json";
const BASE_CHECKS: &str = "base-checks.json";
const RATE_LIMITED: &str = "rate-limited.json";
const REVIEWS: &str = "reviews.json";
const MERGE_BASE_MODIFIED: &str = "merge-base-modified.json";

/// The throwaway pull request whose reviews were recorded (OWL-307), and
/// its repository.
const REVIEWED: u64 = 2;
const SANDBOX: &str = "pitchopp/owlshift-live-sandbox";

/// The pull request of the made-up rate-limited fixture.
const FOLLOWED: u64 = 42;

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
    let set = forge(&replay)
        .with_check_page(1)
        .checks(PR_16, &head())
        .unwrap();
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
    assert!(
        pr.url.ends_with(&format!("/pull/{}", pr.number)),
        "{}",
        pr.url
    );
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

/// Watch's read takes whatever head it finds, with the query's cost: a red
/// check set, a conflict, then the merge (OWL-209).
#[test]
fn watch_reads_a_pull_request_red_conflicting_then_merged() {
    // A fixture recorded live names its own disposable repository and pull
    // request: the read asks for those.
    let read = |name| {
        let replay = Replay::of(&dir(), name);
        let variables = &support::load(&dir(), name).exchanges[0].request["variables"];
        let repo = Repo::parse(&format!(
            "{}/{}",
            variables["owner"].as_str().unwrap(),
            variables["name"].as_str().unwrap()
        ))
        .unwrap();
        let number = variables["number"].as_u64().unwrap();
        let read = GitHubForge::with_transport(replay.clone(), repo)
            .follow(number)
            .unwrap();
        replay.assert_done();
        read
    };
    // Recorded on a disposable repository whose workflow has a passing and
    // a failing job.
    let red = read(FOLLOW_RED);
    assert_eq!(red.checks.state, PrState::Open);
    assert_eq!(red.checks.mergeable, Mergeable::Mergeable);
    assert_eq!(red.checks.merge_state, "UNSTABLE");
    assert_eq!(red.checks.verdict(), Verdict::Red);
    let checks: Vec<_> = red
        .checks
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.state, c.raw.as_str()))
        .collect();
    assert_eq!(
        checks,
        [
            ("passes", CheckState::Passed, "SUCCESS"),
            ("fails", CheckState::Failed, "FAILURE")
        ]
    );
    // A check run's id is its job's, for the fix run's log (OWL-112).
    assert_eq!(red.checks.checks[1].id, Some(114_739_350_663));
    let rate = red.rate.expect("the query's cost");
    assert_eq!((rate.cost, rate.remaining), (1, 4670));

    // The same pull request once its base took a conflicting commit: the
    // failed job is still there, the mergeability changed.
    let conflicting = read(FOLLOW_CONFLICTING);
    assert_eq!(conflicting.checks.state, PrState::Open);
    assert_eq!(conflicting.checks.head, red.checks.head);
    assert_eq!(conflicting.checks.mergeable, Mergeable::Conflicting);
    assert_eq!(conflicting.checks.merge_state, "DIRTY");
    let rate = conflicting.rate.expect("the query's cost");
    assert_eq!((rate.cost, rate.remaining), (1, 4667));

    // PR #16: GitHub still answers its checks, the read lists none.
    let merged = read(FOLLOW_MERGED);
    assert_eq!(merged.checks.state, PrState::Merged);
    assert_eq!(merged.checks.head, head());
    assert!(merged.checks.checks.is_empty());
    assert_eq!(merged.rate.map(|r| r.cost), Some(1));
}

/// The base head's checks and `base_at_push`'s, one per page: a first query
/// for both, then a page query per further check, six in all, their costs
/// summed and the budget left after the last.
#[test]
fn reads_the_checks_of_the_base_head_and_of_base_at_push() {
    let replay = Replay::of(&dir(), BASE_CHECKS);
    let at_push = CommitId::new(FAILED_MAIN).unwrap();
    let base = forge(&replay)
        .with_check_page(1)
        .base_checks(&main_branch(), Some(&at_push))
        .unwrap();
    replay.assert_done();
    assert_eq!(support::load(&dir(), BASE_CHECKS).exchanges.len(), 6);
    fn states(checks: &[owlshift_adapters::forge::Check]) -> Vec<(&str, CheckState)> {
        checks.iter().map(|c| (c.name.as_str(), c.state)).collect()
    }
    // Recorded while the head's Windows job was still running.
    assert_eq!(
        states(&base.at_head),
        [
            ("check (ubuntu-latest)", CheckState::Passed),
            ("check (macos-latest)", CheckState::Passed),
            ("check (windows-latest)", CheckState::Pending)
        ]
    );
    assert_eq!(
        states(base.at_push.as_deref().unwrap()),
        [
            ("check (ubuntu-latest)", CheckState::Passed),
            ("check (macos-latest)", CheckState::Failed),
            ("check (windows-latest)", CheckState::Passed),
            ("ci", CheckState::Failed)
        ]
    );
    let rate = base.rate.unwrap();
    assert_eq!((rate.cost, rate.remaining), (6, 4659));
    assert_eq!(rate.resets_at.to_string(), "2026-10-12T09:43:17Z");
}

/// GraphQL's own limit and a secondary limit both hold the reads, until the
/// reset the answer gives.
#[test]
fn a_spent_budget_is_rate_limited() {
    let replay = Replay::of(&dir(), RATE_LIMITED);
    let forge = forge(&replay);
    let primary = forge.follow(FOLLOWED).unwrap_err();
    assert_eq!(primary.kind, ErrorKind::RateLimited, "{primary}");
    assert_eq!(
        primary.resets_at,
        Some(jiff::Timestamp::from_second(1_791_000_000).unwrap())
    );
    let secondary = forge.follow(FOLLOWED).unwrap_err();
    assert_eq!(secondary.kind, ErrorKind::RateLimited, "{secondary}");
    assert!(secondary.resets_at.is_some());
    replay.assert_done();
}

/// No fixture may hold a token, a header other than the rate limit's, or an
/// e-mail address. In a recorded fixture only the write is made up; a
/// fixture made up whole says so in its `recorded` line and on every
/// exchange.
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
        let made_up = fixture.recorded.starts_with("made up");
        // Only a spent budget (OWL-217) and a merge's race (OWL-444) are
        // made up, and each says why.
        let name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(
            made_up,
            [RATE_LIMITED, MERGE_BASE_MODIFIED].contains(&name),
            "{}",
            path.display()
        );
        assert!(
            !made_up || fixture.recorded.contains("cannot be recorded"),
            "{}: a made-up fixture says why",
            path.display()
        );
        for exchange in &fixture.exchanges {
            // A merge is a REST write, recorded on a disposable repository
            // (OWL-444); a GraphQL write is made up.
            let write = match exchange.request.get("query") {
                Some(query) => query.as_str().unwrap().starts_with("mutation"),
                None => {
                    assert_eq!(exchange.request["method"], "PUT", "{}", path.display());
                    false
                }
            };
            assert_eq!(
                made_up || write,
                exchange.synthesized.is_some(),
                "{}: a GraphQL write must be made up, a read or a merge recorded",
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

/// A `main` commit whose CI failed on its push (2026-10-08): `base_at_push`
/// of the base-checks fixture, for a failed check beside the head's.
const FAILED_MAIN: &str = "e0b59e4a1c954925d980c472a77d915b08567fa7";

/// Records watch's read-only fixtures on `pitchopp/owlshift` (OWL-217): a
/// merged pull request's read and the base's checks, one per page so that
/// the paging is recorded too. `github_live.rs`'s `records_watch_reads`
/// records the red and conflicting reads, which need writes.
///
/// ```sh
/// cargo test -p owlshift-adapters --test github -- --ignored --nocapture record_watch_fixtures
/// ```
#[test]
#[ignore = "records against the live GitHub API"]
fn record_watch_fixtures() {
    let token = support::github_token();
    let recorded = format!(
        "{} on pitchopp/owlshift, recorded live",
        jiff::Zoned::now().date()
    );
    let recorder = || Recorder::over(HttpTransport::new(token.clone()), dir());

    let r = recorder();
    let merged = GitHubForge::with_transport(r.clone(), repo())
        .follow(PR_16)
        .unwrap();
    assert_eq!(merged.checks.state, PrState::Merged);
    let rate = merged.rate.expect("rateLimit in watch's query");
    eprintln!("merged: cost {} points, {} left", rate.cost, rate.remaining);
    r.write(FOLLOW_MERGED, &recorded);

    let r = recorder();
    let at_push = CommitId::new(FAILED_MAIN).unwrap();
    let base = GitHubForge::with_transport(r.clone(), repo())
        .with_check_page(1)
        .base_checks(&main_branch(), Some(&at_push))
        .unwrap();
    let at_push = base.at_push.expect("the failed commit's checks");
    assert!(base.at_head.len() > 1, "{:?}", base.at_head);
    assert!(
        at_push.iter().any(|c| c.state == CheckState::Failed),
        "{at_push:?}"
    );
    let rate = base.rate.expect("rateLimit in watch's queries");
    eprintln!(
        "base checks: {} checks at head, {} at push, cost {} points, {} left",
        base.at_head.len(),
        at_push.len(),
        rate.cost,
        rate.remaining
    );
    r.write(BASE_CHECKS, &recorded);
}

/// The reviews of a pull request in one query (OWL-307): ids past 2^32,
/// the author's login, a body or none, an outdated inline comment without
/// its line, and a reply in a thread read as a review of its own.
#[test]
fn reads_the_reviews_of_a_pull_request_with_their_inline_comments() {
    let replay = Replay::of(&dir(), REVIEWS);
    let read = GitHubForge::with_transport(replay.clone(), Repo::parse(SANDBOX).unwrap())
        .reviews(REVIEWED)
        .unwrap();
    replay.assert_done();
    let rate = read.rate.expect("rateLimit in the reviews' query");
    assert_eq!(rate.cost, 1);
    let [first, inline, reply] = &read.reviews[..] else {
        panic!("{:?}", read.reviews);
    };
    for review in &read.reviews {
        assert!(review.id > u64::from(u32::MAX), "{review:?}");
        assert_eq!(review.author.as_deref(), Some("pitchopp"));
        assert_eq!(review.state, "COMMENTED");
    }
    assert_eq!(first.body, "Please rename the file.");
    assert_eq!(first.comments.len(), 1);
    assert_eq!(first.comments[0].path, "owl-307-review.txt");
    assert_eq!(first.comments[0].line, None, "outdated by a later push");
    assert_eq!(first.comments[0].body, "Say two in digits.");
    assert_eq!(inline.body, "");
    assert_eq!(inline.comments[0].line, Some(3));
    assert_eq!(reply.comments[0].body, "A reply in the thread.");
    assert!(first.submitted_at < inline.submitted_at && inline.submitted_at < reply.submitted_at);
    assert!(first.commit.is_some());
}

/// The reviews' query names every submitted state, so a pending review,
/// which GitHub shows its own author, is never read (checked live on
/// 2026-10-16), and an answer without a submission time is dropped.
#[test]
fn a_pending_review_is_never_read() {
    let replay = Replay::of(&dir(), REVIEWS);
    GitHubForge::with_transport(replay.clone(), Repo::parse(SANDBOX).unwrap())
        .reviews(REVIEWED)
        .unwrap();
    let fixture = support::load(&dir(), REVIEWS);
    let query = fixture.exchanges[0].request["query"].as_str().unwrap();
    assert!(
        query.contains("states: [COMMENTED, APPROVED, CHANGES_REQUESTED, DISMISSED]"),
        "{query}"
    );
    let mut answer = fixture.exchanges[0].response.clone();
    let nodes = &mut answer["data"]["repository"]["pullRequest"]["reviews"]["nodes"];
    nodes[0]["submittedAt"] = Value::Null;
    nodes[0]["state"] = json!("PENDING");
    let mut exchange = fixture.exchanges[0].clone();
    exchange.response = answer;
    let pending = support::Replay::from_exchanges("pending", vec![exchange]);
    let read = GitHubForge::with_transport(pending.clone(), Repo::parse(SANDBOX).unwrap())
        .reviews(REVIEWED)
        .unwrap();
    pending.assert_done();
    assert_eq!(read.reviews.len(), 2);
    assert!(read.reviews.iter().all(|review| review.state != "PENDING"));
}

/// Records the reviews' read on the sandbox's throwaway pull request #2
/// (OWL-307), read-only: its reviews were made for the check (c) of the
/// build plan, and the pull request is closed.
///
/// ```sh
/// cargo test -p owlshift-adapters --test github -- --ignored --nocapture record_review_fixtures
/// ```
#[test]
#[ignore = "records against the live GitHub API"]
fn record_review_fixtures() {
    let token = support::github_token();
    let recorded = format!("{} on {SANDBOX}, recorded live", jiff::Zoned::now().date());
    let r = Recorder::over(HttpTransport::new(token), dir());
    let read = GitHubForge::with_transport(r.clone(), Repo::parse(SANDBOX).unwrap())
        .reviews(REVIEWED)
        .unwrap();
    assert_eq!(read.reviews.len(), 3, "{:?}", read.reviews);
    r.write(REVIEWS, &recorded);
}

/// What each merge fixture must read as (OWL-444): every `merge-*.json`,
/// replayed through the forge, so the request it sends is the recorded one
/// too. A new recording fails here until it is given its answer.
#[test]
fn reads_every_recorded_merge_answer() {
    let expected = [
        ("merge-merged.json", "merged"),
        ("merge-already-merged.json", "merged"),
        ("merge-closed.json", "merged"),
        ("merge-head-moved.json", "head moved"),
        ("merge-conflict.json", "refused, may pass"),
        (MERGE_BASE_MODIFIED, "refused, may pass"),
        ("merge-method-forbidden.json", "refused, will not pass"),
        ("merge-review-required.json", "refused, will not pass"),
        ("merge-check-required.json", "refused, will not pass"),
        ("merge-draft.json", "refused, will not pass"),
        // An administrator's token passes a review the protection does not
        // enforce for administrators (C19).
        ("merge-review-admin.json", "merged"),
    ];
    let mut names: Vec<String> = fs::read_dir(dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("merge-"))
        .collect();
    names.sort();
    let mut listed: Vec<&str> = expected.iter().map(|(name, _)| *name).collect();
    listed.sort_unstable();
    assert_eq!(names, listed, "every merge fixture has its expected answer");
    for (name, kind) in expected {
        let fixture = support::load(&dir(), name);
        let exchange = &fixture.exchanges[0];
        let request = &exchange.request;
        let (repo, number) = request["path"]
            .as_str()
            .and_then(|path| path.strip_prefix("repos/"))
            .and_then(|rest| rest.strip_suffix("/merge"))
            .and_then(|rest| rest.split_once("/pulls/"))
            .unwrap();
        let method = match request["body"]["merge_method"].as_str().unwrap() {
            "merge" => MergeMethod::Merge,
            "squash" => MergeMethod::Squash,
            "rebase" => MergeMethod::Rebase,
            other => panic!("{name}: {other}"),
        };
        let sha = CommitId::new(request["body"]["sha"].as_str().unwrap()).unwrap();
        let replay = Replay::of(&dir(), name);
        let answer = GitHubForge::with_transport(replay.clone(), Repo::parse(repo).unwrap())
            .merge_pull_request(number.parse().unwrap(), &sha, method)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        replay.assert_done();
        let read = match &answer {
            MergeAnswer::Merged { commit } => {
                assert_eq!(Some(commit.as_str()), exchange.response["sha"].as_str());
                "merged"
            }
            MergeAnswer::HeadMoved { .. } => "head moved",
            MergeAnswer::Refused(refusal) => {
                assert_eq!(refusal.status, exchange.status, "{name}");
                assert_eq!(
                    Some(refusal.message.as_str()),
                    exchange.response["message"].as_str(),
                    "{name}"
                );
                if refusal.may_pass {
                    "refused, may pass"
                } else {
                    "refused, will not pass"
                }
            }
            MergeAnswer::NoVerdict { .. } => "no verdict",
        };
        assert_eq!(read, kind, "{name}: {answer:?}");
    }
}
