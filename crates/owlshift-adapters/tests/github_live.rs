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
//! The repository needs a `main` branch. The token comes from
//! `OWLSHIFT_GITHUB_TOKEN`, or from the `gh` login; the push uses your own
//! git credentials over SSH, or over `OWLSHIFT_LIVE_GITHUB_REMOTE` when set.

mod support;

use std::path::Path;
use std::process::Command;

use serde_json::json;

use owlshift_adapters::forge::github::{GitHubForge, HttpTransport, NewPullRequest, Transport};
use owlshift_adapters::forge::push::{Pushed, push_command, read_push};
use owlshift_adapters::forge::{Branch, CommitId, ErrorKind, PrState, Repo};

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
