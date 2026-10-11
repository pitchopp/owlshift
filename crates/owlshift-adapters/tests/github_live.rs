//! The GitHub forge adapter end to end on a real, disposable test
//! repository: push a branch, open a pull request, find it, read its checks,
//! then close it and delete the branch. It writes to GitHub, so it never runs
//! in CI and names its repository explicitly:
//!
//! ```sh
//! OWLSHIFT_LIVE_GITHUB=<owner>/<test-repo> \
//!   cargo test -p owlshift-adapters --test github_live -- --ignored --nocapture
//! ```
//!
//! `records_watch_reads` (OWL-209) records watch's read of a conflicting
//! pull request, with the query's cost, as `tests/fixtures/github/
//! follow-conflicting.json`: the live checks (a) and (b) of the build plan's
//! "Watch after delivery". It pushes a commit to the repository's `main` to
//! make the conflict, so the repository must be one nobody else uses:
//!
//! ```sh
//! OWLSHIFT_LIVE_GITHUB=<owner>/<test-repo> \
//!   cargo test -p owlshift-adapters --test github_live -- --ignored --nocapture records_watch_reads
//! ```
//!
//! The repository needs a `main` branch. The token comes from
//! `OWLSHIFT_GITHUB_TOKEN`, or from the `gh` login; the push uses your own
//! git credentials over SSH, or over `OWLSHIFT_LIVE_GITHUB_REMOTE` when set.

mod support;

use std::path::Path;
use std::process::Command;

use serde_json::json;

use owlshift_adapters::forge::github::{GitHubForge, HttpTransport, NewPullRequest, Transport};
use owlshift_adapters::forge::push::{Pushed, push_command, read_push};
use owlshift_adapters::forge::{Branch, CommitId, ErrorKind, Mergeable, PrState, Repo};

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Closes the pull request and deletes the branch, even when the test fails
/// half-way.
struct Cleanup<'a> {
    checkout: &'a Path,
    branch: &'a Branch,
    pushed: bool,
    pull_request_id: Option<String>,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        if let Some(id) = &self.pull_request_id {
            let answer = HttpTransport::new(support::github_token()).send(&json!({
                "query": "mutation Close($id: ID!) { closePullRequest(input: { pullRequestId: $id }) { pullRequest { state } } }",
                "variables": { "id": id },
            }));
            eprintln!("closing the pull request: {answer:?}");
        }
        if self.pushed {
            let status = Command::new("git")
                .arg("-C")
                .arg(self.checkout)
                .args([
                    "push",
                    "--quiet",
                    "origin",
                    "--delete",
                    self.branch.as_str(),
                ])
                .status();
            eprintln!("deleting {}: {status:?}", self.branch);
        }
    }
}

#[test]
#[ignore = "live: writes to GitHub; set OWLSHIFT_LIVE_GITHUB=<owner>/<test-repo>"]
fn delivers_a_branch_as_a_pull_request_on_a_test_repository() {
    let repo = std::env::var("OWLSHIFT_LIVE_GITHUB")
        .expect("this test writes to GitHub: name a disposable repository in OWLSHIFT_LIVE_GITHUB");
    let repo = Repo::parse(&repo).unwrap();
    let remote = std::env::var("OWLSHIFT_LIVE_GITHUB_REMOTE")
        .unwrap_or_else(|_| format!("git@github.com:{repo}.git"));
    let forge = GitHubForge::new(support::github_token(), repo.clone());

    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let status = Command::new("git")
        .args([
            "clone", "--quiet", "--depth", "1", "--branch", "main", &remote,
        ])
        .arg(&checkout)
        .status()
        .unwrap();
    assert!(status.success(), "cloning {remote}");
    std::fs::write(checkout.join("owlshift-live.txt"), "live test\n").unwrap();
    git(&checkout, &["add", "owlshift-live.txt"]);
    git(
        &checkout,
        &[
            "-c",
            "user.name=Owlshift live test",
            "-c",
            "user.email=live@owlshift.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "Owlshift live test",
        ],
    );
    let commit = CommitId::new(git(&checkout, &["rev-parse", "HEAD"])).unwrap();
    let branch = Branch::new(format!(
        "owlshift-live-{}",
        jiff::Timestamp::now().as_second()
    ))
    .unwrap();
    let base = Branch::new("main").unwrap();
    let mut cleanup = Cleanup {
        checkout: &checkout,
        branch: &branch,
        pushed: false,
        pull_request_id: None,
    };

    let mut push = push_command(Path::new("git"), &checkout, "origin", &commit, &branch).unwrap();
    let pushed = read_push(&branch, &push.output().unwrap());
    cleanup.pushed = pushed.is_ok();
    assert_eq!(pushed, Ok(Pushed::Created));

    assert_eq!(forge.find_open_pull_request(&branch, &base).unwrap(), None);
    let body = "Opened by the Owlshift live test.\n\n- a list\n- kept verbatim\n";
    let pr = forge
        .open_pull_request(NewPullRequest {
            head: &branch,
            base: &base,
            title: "Owlshift live test",
            body,
            draft: false,
        })
        .unwrap();
    eprintln!("opened {}", pr.url);
    cleanup.pull_request_id = Some(node_id(&repo, pr.number));
    assert_eq!(pr.state, PrState::Open);
    assert_eq!(pr.head, commit);
    assert_eq!(
        forge.find_open_pull_request(&branch, &base).unwrap(),
        Some(pr.clone())
    );

    let set = forge.checks(pr.number, &commit).unwrap();
    eprintln!(
        "{} checks, mergeable {:?}, merge state {}, verdict {:?}",
        set.checks.len(),
        set.mergeable,
        set.merge_state,
        set.verdict()
    );
    assert_eq!(set.head, commit);
    assert_eq!(set.state, PrState::Open);

    let other = CommitId::new("0".repeat(40)).unwrap();
    let moved = forge.checks(pr.number, &other).unwrap_err();
    assert_eq!(moved.kind, ErrorKind::HeadMoved, "{moved}");
}

/// The pull request's GraphQL id, for closing it.
fn node_id(repo: &Repo, number: u64) -> String {
    let answer = HttpTransport::new(support::github_token())
        .send(&json!({
            "query": "query Id($owner: String!, $name: String!, $number: Int!) { repository(owner: $owner, name: $name) { pullRequest(number: $number) { id } } }",
            "variables": { "owner": repo.owner(), "name": repo.name(), "number": number },
        }))
        .unwrap();
    let answer: serde_json::Value = serde_json::from_str(&answer.body).unwrap();
    answer["data"]["repository"]["pullRequest"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Commits `content` as `file` in `checkout` and returns the commit.
fn commit_file(checkout: &Path, file: &str, content: &str, message: &str) -> CommitId {
    std::fs::write(checkout.join(file), content).unwrap();
    git(checkout, &["add", file]);
    git(
        checkout,
        &[
            "-c",
            "user.name=Owlshift live test",
            "-c",
            "user.email=live@owlshift.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
    CommitId::new(git(checkout, &["rev-parse", "HEAD"])).unwrap()
}

/// OWL-209's live checks (a) and (b): a pull request made to conflict with
/// its base reads `CONFLICTING` through watch's query, whose `rateLimit`
/// gives its real cost; the read is recorded as the fixture the contract
/// test replays. Only the repository `OWLSHIFT_LIVE_GITHUB` names is
/// touched: its `main` gets one commit, and the pull request is closed and
/// its branch deleted at the end.
#[test]
#[ignore = "live: writes to GitHub; set OWLSHIFT_LIVE_GITHUB=<owner>/<test-repo>"]
fn records_watch_reads() {
    let repo = std::env::var("OWLSHIFT_LIVE_GITHUB")
        .expect("this test writes to GitHub: name a disposable repository in OWLSHIFT_LIVE_GITHUB");
    let repo = Repo::parse(&repo).unwrap();
    assert_ne!(
        repo.to_string(),
        "pitchopp/owlshift",
        "records_watch_reads pushes to main: name a disposable repository"
    );
    let remote = std::env::var("OWLSHIFT_LIVE_GITHUB_REMOTE")
        .unwrap_or_else(|_| format!("git@github.com:{repo}.git"));
    let token = support::github_token();
    let forge = GitHubForge::new(token.clone(), repo.clone());

    let dir = tempfile::tempdir().unwrap();
    let checkout = dir.path().join("checkout");
    let status = Command::new("git")
        .args(["clone", "--quiet", "--branch", "main", &remote])
        .arg(&checkout)
        .status()
        .unwrap();
    assert!(status.success(), "cloning {remote}");
    let base_commit = git(&checkout, &["rev-parse", "HEAD"]);
    let stamp = jiff::Timestamp::now().as_second();
    let branch = Branch::new(format!("owlshift-live-conflict-{stamp}")).unwrap();
    let base = Branch::new("main").unwrap();
    let file = "owlshift-live-conflict.txt";

    let commit = commit_file(
        &checkout,
        file,
        "the pull request's line\n",
        "The pull request's side",
    );
    let mut cleanup = Cleanup {
        checkout: &checkout,
        branch: &branch,
        pushed: false,
        pull_request_id: None,
    };
    let mut push = push_command(Path::new("git"), &checkout, "origin", &commit, &branch).unwrap();
    let pushed = read_push(&branch, &push.output().unwrap());
    cleanup.pushed = pushed.is_ok();
    assert_eq!(pushed, Ok(Pushed::Created));
    let pr = forge
        .open_pull_request(NewPullRequest {
            head: &branch,
            base: &base,
            title: "Owlshift live test: a conflict",
            body: "Opened by the Owlshift live test; closed at its end.\n",
            draft: false,
        })
        .unwrap();
    eprintln!("opened {}", pr.url);
    cleanup.pull_request_id = Some(node_id(&repo, pr.number));

    // The base's side of the same file, pushed to main.
    git(
        &checkout,
        &["checkout", "--quiet", "--detach", &base_commit],
    );
    let theirs = commit_file(&checkout, file, "the base's line\n", "The base's side");
    git(
        &checkout,
        &[
            "push",
            "--quiet",
            "origin",
            &format!("{theirs}:refs/heads/main"),
        ],
    );

    // GitHub computes mergeability in the background.
    let mut read = forge.follow(pr.number).unwrap();
    for _ in 0..30 {
        if read.checks.mergeable != Mergeable::Unknown {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(5));
        read = forge.follow(pr.number).unwrap();
    }
    eprintln!(
        "mergeable {:?}, merge state {}, cost {:?}",
        read.checks.mergeable, read.checks.merge_state, read.rate
    );
    assert_eq!(read.checks.mergeable, Mergeable::Conflicting);

    let recorded = format!("{} on {repo}, recorded live", jiff::Zoned::now().date());
    let recorder = support::Recorder::over(HttpTransport::new(token), support::fixtures("github"));
    let read = GitHubForge::with_transport(recorder.clone(), repo.clone())
        .follow(pr.number)
        .unwrap();
    assert_eq!(read.checks.mergeable, Mergeable::Conflicting);
    let rate = read.rate.expect("rateLimit in watch's query");
    eprintln!("the follow query cost {} points", rate.cost);
    recorder.write("follow-conflicting.json", &recorded);
}

/// Live check (c) of the build plan's "Watch after delivery" (OWL-112):
/// whether the token Owlshift stores reads a failed job's log. Read only,
/// so any repository will do, `pitchopp/owlshift` included; pass the stored
/// token in `OWLSHIFT_GITHUB_TOKEN`, since `gh`'s own login is not what
/// Owlshift reads:
///
/// ```sh
/// OWLSHIFT_GITHUB_TOKEN=<the stored token> OWLSHIFT_LIVE_GITHUB=pitchopp/owlshift \
///   OWLSHIFT_LIVE_GITHUB_JOB=110572493177 \
///   cargo test -p owlshift-adapters --test github_live -- --ignored --nocapture reads_a_failed_jobs_log
/// ```
///
/// A refused token fails with `Unauthorized`: the token lacks actions (read).
#[test]
#[ignore = "live: reads GitHub; set OWLSHIFT_LIVE_GITHUB and OWLSHIFT_LIVE_GITHUB_JOB"]
fn reads_a_failed_jobs_log() {
    let repo = Repo::parse(&std::env::var("OWLSHIFT_LIVE_GITHUB").unwrap()).unwrap();
    let job: u64 = std::env::var("OWLSHIFT_LIVE_GITHUB_JOB")
        .unwrap()
        .parse()
        .unwrap();
    let forge = GitHubForge::new(support::github_token(), repo);
    match forge.job_log_excerpt(job) {
        Ok(excerpt) => {
            eprintln!("{} bytes of excerpt:\n{excerpt}", excerpt.len());
            assert!(!excerpt.is_empty());
        }
        Err(error) => panic!("{:?}: {error}", error.kind),
    }
}
