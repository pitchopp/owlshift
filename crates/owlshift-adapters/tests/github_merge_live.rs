//! The GitHub forge adapter's merge at an expected head (OWL-444), live:
//! each answer GitHub gives the merge is recorded as the fixture
//! `tests/github.rs` replays (`tests/fixtures/github/merge-*.json`). It
//! merges pull requests, changes repository settings and branch protection,
//! so it never runs in CI and each test names its repository explicitly.
//!
//! `records_merge_answers` runs on a repository with no branch protection:
//! a merge at a stale head (409) then at the head (200), the same merge
//! again, a method the repository forbids (`allow_rebase_merge` turned off
//! for the call, then on again), a pull request in conflict, and a closed
//! one. Its `main` gets one commit, for the conflict.
//!
//! ```sh
//! OWLSHIFT_LIVE_GITHUB=<owner>/<test-repo> \
//!   cargo test -p owlshift-adapters --test github_merge_live -- --ignored --nocapture records_merge_answers
//! ```
//!
//! `records_protection_refusals` needs branch protection, which GitHub's
//! free plan gives public repositories only: a required review, enforced
//! for administrators then not, a required status check, and a draft. It
//! removes every protection of `main` first and last, so a run killed
//! half-way is undone by the next.
//!
//! ```sh
//! OWLSHIFT_LIVE_GITHUB_PROTECTED=<owner>/<public-test-repo> \
//!   cargo test -p owlshift-adapters --test github_merge_live -- --ignored --nocapture records_protection_refusals
//! ```
//!
//! `records_merge_reads` (OWL-446) records the last read before a merge as
//! `tests/fixtures/github/before-merge-*.json` on the same public repository:
//! one pull request open, under a required review not enforced for
//! administrators, then closed.
//!
//! ```sh
//! OWLSHIFT_LIVE_GITHUB_PROTECTED=<owner>/<public-test-repo> \
//!   cargo test -p owlshift-adapters --test github_merge_live -- --ignored --nocapture records_merge_reads
//! ```
//!
//! All need a `main` branch, and the token of `OWLSHIFT_GITHUB_TOKEN` or the
//! `gh` login, an administrator of the repository; the setup goes through
//! `gh api` and that same login. Only statuses and GitHub's messages are
//! printed, never a token.

mod support;

use std::process::{Command, Stdio};

use serde_json::{Value, json};

use owlshift_adapters::forge::github::{GitHubForge, HttpTransport};
use owlshift_adapters::forge::{
    CommitId, MergeAnswer, MergeMethod, MergeRead, PrState, Repo, ReviewDecision,
};

use support::Recorder;

/// `gh api` on `path` with `method`, and `body` on its standard input. The
/// answer's JSON, and whether `gh` succeeded (an HTTP error fails it).
fn gh(method: &str, path: &str, body: Option<&Value>) -> (bool, Value) {
    let mut child = Command::new("gh")
        .args(["api", "--method", method, path])
        .args(body.map(|_| ["--input", "-"]).into_iter().flatten())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the gh CLI, logged in");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        if let Some(body) = body {
            stdin.write_all(body.to_string().as_bytes()).unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    let answer = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (output.status.success(), answer)
}

/// `gh api` that must succeed.
fn gh_ok(method: &str, path: &str, body: Option<&Value>) -> Value {
    let (ok, answer) = gh(method, path, body);
    assert!(ok, "gh api {method} {path}: {}", answer["message"]);
    answer
}

/// Standard base64, for the contents API.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn base64_encodes_as_the_standard_says() {
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"f"), "Zg==");
    assert_eq!(base64(b"fo"), "Zm8=");
    assert_eq!(base64(b"foo"), "Zm9v");
    assert_eq!(base64(b"foobar"), "Zm9vYmFy");
}

/// One live repository, the pull requests and branches a test made in it,
/// closed and deleted at the end even when the test fails half-way.
struct Sandbox {
    repo: Repo,
    stamp: i64,
    branches: Vec<String>,
    pulls: Vec<u64>,
    protected: bool,
}

impl Sandbox {
    fn new(var: &str) -> Self {
        let repo = std::env::var(var).unwrap_or_else(|_| {
            panic!("this test writes to GitHub: name a disposable repository in {var}")
        });
        Self {
            repo: Repo::parse(&repo).unwrap(),
            stamp: jiff::Timestamp::now().as_second(),
            branches: Vec::new(),
            pulls: Vec::new(),
            protected: false,
        }
    }

    fn api(&self, path: &str) -> String {
        format!("repos/{}/{}{path}", self.repo.owner(), self.repo.name())
    }

    fn main(&self) -> CommitId {
        let answer = gh_ok("GET", &self.api("/git/ref/heads/main"), None);
        CommitId::new(answer["object"]["sha"].as_str().unwrap()).unwrap()
    }

    /// Commits `content` as `file` on `branch`, and returns the commit.
    fn commit(&self, branch: &str, file: &str, content: &str) -> CommitId {
        let answer = gh_ok(
            "PUT",
            &self.api(&format!("/contents/{file}")),
            Some(&json!({
                "message": format!("Owlshift merge fixture: {file}"),
                "content": base64(content.as_bytes()),
                "branch": branch,
            })),
        );
        CommitId::new(answer["commit"]["sha"].as_str().unwrap()).unwrap()
    }

    /// A branch off `main` that adds `file`, and its pull request into
    /// `main`: its number and head.
    fn pull_request(&mut self, name: &str, file: &str, draft: bool) -> (u64, CommitId) {
        let branch = format!("owlshift-merge-{name}-{}", self.stamp);
        let base = self.main();
        gh_ok(
            "POST",
            &self.api("/git/refs"),
            Some(&json!({ "ref": format!("refs/heads/{branch}"), "sha": base.as_str() })),
        );
        self.branches.push(branch.clone());
        let head = self.commit(&branch, file, &format!("{name}\n"));
        let answer = gh_ok(
            "POST",
            &self.api("/pulls"),
            Some(&json!({
                "title": format!("Owlshift merge fixture: {name}"),
                "head": branch,
                "base": "main",
                "body": "Opened and closed by the Owlshift merge fixture recorder (OWL-444).",
                "draft": draft,
            })),
        );
        let number = answer["number"].as_u64().unwrap();
        self.pulls.push(number);
        (number, head)
    }

    /// Records the merge of `number` at `sha` by `method` as `fixture`.
    fn merge(
        &self,
        fixture: &str,
        what: &str,
        number: u64,
        sha: &CommitId,
        method: MergeMethod,
    ) -> MergeAnswer {
        let recorder = Recorder::over(
            HttpTransport::new(support::github_token()),
            support::fixtures("github"),
        );
        let forge = GitHubForge::with_transport(recorder.clone(), self.repo.clone());
        let answer = forge
            .merge_pull_request(number, sha, method)
            .unwrap_or_else(|e| panic!("{fixture}: {:?}: {e}", e.kind));
        let date = jiff::Timestamp::now().strftime("%Y-%m-%d");
        recorder.write(
            fixture,
            &format!(
                "{date} on {}, recorded live with a personal token (the gh login, an administrator): {what}",
                self.repo
            ),
        );
        eprintln!("{fixture}: {answer:?}");
        answer
    }

    /// Records the last read before a merge of `number` as `fixture`
    /// (OWL-446).
    fn read(&self, fixture: &str, what: &str, number: u64) -> MergeRead {
        let recorder = Recorder::over(
            HttpTransport::new(support::github_token()),
            support::fixtures("github"),
        );
        let forge = GitHubForge::with_transport(recorder.clone(), self.repo.clone());
        let read = forge
            .merge_read(number)
            .unwrap_or_else(|e| panic!("{fixture}: {:?}: {e}", e.kind));
        let date = jiff::Timestamp::now().strftime("%Y-%m-%d");
        recorder.write(
            fixture,
            &format!(
                "{date} on {}, recorded live with a personal token (the gh login, an administrator): {what}",
                self.repo
            ),
        );
        eprintln!(
            "{fixture}: {:?} {:?} draft {}",
            read.state, read.review, read.draft
        );
        read
    }

    /// Whether GitHub has computed the pull request's mergeability, polled
    /// for up to a minute: its `mergeable` once known.
    fn mergeable(&self, number: u64) -> Option<bool> {
        for _ in 0..30 {
            let answer = gh_ok("GET", &self.api(&format!("/pulls/{number}")), None);
            if let Some(mergeable) = answer["mergeable"].as_bool() {
                return Some(mergeable);
            }
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        None
    }

    fn unprotect(&self) {
        // 404 when `main` has no protection: nothing to undo.
        let _ = gh("DELETE", &self.api("/branches/main/protection"), None);
    }

    fn protect(&mut self, protection: &Value) {
        self.protected = true;
        gh_ok(
            "PUT",
            &self.api("/branches/main/protection"),
            Some(protection),
        );
    }

    fn close(&self, number: u64) {
        let _ = gh(
            "PATCH",
            &self.api(&format!("/pulls/{number}")),
            Some(&json!({ "state": "closed" })),
        );
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if self.protected {
            self.unprotect();
        }
        let _ = gh(
            "PATCH",
            &self.api(""),
            Some(&json!({ "allow_rebase_merge": true })),
        );
        for number in &self.pulls {
            self.close(*number);
        }
        for branch in &self.branches {
            let _ = gh(
                "DELETE",
                &self.api(&format!("/git/refs/heads/{branch}")),
                None,
            );
        }
        eprintln!("cleaned up {}", self.repo);
    }
}

fn kind(answer: &MergeAnswer) -> &'static str {
    match answer {
        MergeAnswer::Merged { .. } => "merged",
        MergeAnswer::HeadMoved { .. } => "head moved",
        MergeAnswer::Refused(refusal) if refusal.may_pass => "refused, may pass",
        MergeAnswer::Refused(_) => "refused, will not pass",
        MergeAnswer::NoVerdict { .. } => "no verdict",
    }
}

/// The answers of a repository without protection (C19, OWL-444).
#[test]
#[ignore = "live: merges on GitHub; set OWLSHIFT_LIVE_GITHUB=<owner>/<test-repo>"]
fn records_merge_answers() {
    let mut sandbox = Sandbox::new("OWLSHIFT_LIVE_GITHUB");
    let stamp = sandbox.stamp;
    // A run killed half-way may have left rebase merges off.
    gh_ok(
        "PATCH",
        &sandbox.api(""),
        Some(&json!({ "allow_rebase_merge": true })),
    );

    let (merged, head) = sandbox.pull_request("merged", &format!("merged-{stamp}.txt"), false);
    let stale = sandbox.main();
    let answer = sandbox.merge(
        "merge-head-moved.json",
        "a merge whose sha is the base, not the head",
        merged,
        &stale,
        MergeMethod::Squash,
    );
    assert_eq!(kind(&answer), "head moved");
    let answer = sandbox.merge(
        "merge-merged.json",
        "a squash merge at the head",
        merged,
        &head,
        MergeMethod::Squash,
    );
    assert_eq!(kind(&answer), "merged");
    sandbox.merge(
        "merge-already-merged.json",
        "the same merge again, once merged",
        merged,
        &head,
        MergeMethod::Squash,
    );

    let (forbidden, head) =
        sandbox.pull_request("forbidden", &format!("forbidden-{stamp}.txt"), false);
    gh_ok(
        "PATCH",
        &sandbox.api(""),
        Some(&json!({ "allow_rebase_merge": false })),
    );
    let answer = sandbox.merge(
        "merge-method-forbidden.json",
        "a rebase merge on a repository that allows none",
        forbidden,
        &head,
        MergeMethod::Rebase,
    );
    gh_ok(
        "PATCH",
        &sandbox.api(""),
        Some(&json!({ "allow_rebase_merge": true })),
    );
    assert_eq!(kind(&answer), "refused, will not pass");

    sandbox.close(forbidden);
    sandbox.merge(
        "merge-closed.json",
        "a merge of a closed pull request",
        forbidden,
        &head,
        MergeMethod::Squash,
    );

    // Both sides add the same file with different contents.
    let file = format!("conflict-{stamp}.txt");
    let (conflict, head) = sandbox.pull_request("conflict", &file, false);
    sandbox.commit("main", &file, "main's side\n");
    eprintln!("mergeable: {:?}", sandbox.mergeable(conflict));
    sandbox.merge(
        "merge-conflict.json",
        "a merge of a pull request in conflict with its base",
        conflict,
        &head,
        MergeMethod::Squash,
    );
}

/// The refusals of branch protection (C19, OWL-444).
#[test]
#[ignore = "live: merges and protects on GitHub; set OWLSHIFT_LIVE_GITHUB_PROTECTED=<owner>/<public-test-repo>"]
fn records_protection_refusals() {
    let mut sandbox = Sandbox::new("OWLSHIFT_LIVE_GITHUB_PROTECTED");
    let stamp = sandbox.stamp;
    sandbox.unprotect();

    let (reviewed, head) = sandbox.pull_request("review", &format!("review-{stamp}.txt"), false);
    sandbox.protect(&json!({
        "required_status_checks": null,
        "enforce_admins": true,
        "required_pull_request_reviews": { "required_approving_review_count": 1 },
        "restrictions": null,
    }));
    sandbox.merge(
        "merge-review-required.json",
        "a merge that branch protection requires a review for, enforced for administrators",
        reviewed,
        &head,
        MergeMethod::Squash,
    );
    gh_ok(
        "DELETE",
        &sandbox.api("/branches/main/protection/enforce_admins"),
        None,
    );
    sandbox.merge(
        "merge-review-admin.json",
        "the same merge, the protection no longer enforced for administrators",
        reviewed,
        &head,
        MergeMethod::Squash,
    );

    let (checked, head) = sandbox.pull_request("check", &format!("check-{stamp}.txt"), false);
    sandbox.protect(&json!({
        "required_status_checks": { "strict": false, "contexts": ["owlshift-fixture"] },
        "enforce_admins": true,
        "required_pull_request_reviews": null,
        "restrictions": null,
    }));
    sandbox.merge(
        "merge-check-required.json",
        "a merge that branch protection requires a status check for, never reported",
        checked,
        &head,
        MergeMethod::Squash,
    );
    sandbox.unprotect();

    let (draft, head) = sandbox.pull_request("draft", &format!("draft-{stamp}.txt"), true);
    sandbox.merge(
        "merge-draft.json",
        "a merge of a draft pull request",
        draft,
        &head,
        MergeMethod::Squash,
    );
}

/// The last read before a merge (OWL-446, C19): one pull request read open
/// without protection, under a required review not enforced for
/// administrators, which `reviewDecision` reads as required though this
/// token merges past it, and closed.
#[test]
#[ignore = "live: protects on GitHub; set OWLSHIFT_LIVE_GITHUB_PROTECTED=<owner>/<public-test-repo>"]
fn records_merge_reads() {
    let mut sandbox = Sandbox::new("OWLSHIFT_LIVE_GITHUB_PROTECTED");
    let stamp = sandbox.stamp;
    sandbox.unprotect();

    let (number, head) = sandbox.pull_request("read", &format!("read-{stamp}.txt"), false);
    let base = sandbox.main();
    let read = sandbox.read(
        "before-merge-open.json",
        "an open pull request, no protection",
        number,
    );
    assert_eq!(
        (read.state, read.review, &read.head, &read.base_head),
        (PrState::Open, ReviewDecision::NotRequired, &head, &base)
    );
    sandbox.protect(&json!({
        "required_status_checks": null,
        "enforce_admins": false,
        "required_pull_request_reviews": { "required_approving_review_count": 1 },
        "restrictions": null,
    }));
    // GitHub takes a moment to apply a protection to its pull requests.
    std::thread::sleep(std::time::Duration::from_secs(3));
    let read = sandbox.read(
        "before-merge-review-required.json",
        "the same pull request, a review required but not enforced for administrators",
        number,
    );
    assert_eq!(read.review, ReviewDecision::Required);
    sandbox.unprotect();
    sandbox.close(number);
    let read = sandbox.read(
        "before-merge-closed.json",
        "the same pull request, closed",
        number,
    );
    assert_eq!(read.state, PrState::Closed);
}
