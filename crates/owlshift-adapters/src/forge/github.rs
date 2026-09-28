//! The GitHub forge: GitHub's GraphQL API over HTTPS, authenticated with a
//! token that the runner reads from the system keychain. Pushing goes
//! through git ([`crate::forge::push`]), with the user's own git credentials.
//!
//! The adapter talks through a [`Transport`] ([`crate::graphql`]):
//! [`HttpTransport`] in production, a replay of recorded exchanges in tests.
//!
//! What the adapter relies on was checked live on `pitchopp/owlshift` on
//! 2026-09-28 (OWL-17; build plan, "The GitHub forge adapter"):
//!
//! - `commit.statusCheckRollup.contexts` lists the check runs of every app
//!   and the commit statuses of a commit in one paginated connection
//!   (`CheckRun | StatusContext`), with `totalCount` and
//!   `isRequired(pullRequestNumber:)`; it is null when the commit has none.
//! - `PullRequest.mergeable` is `MERGEABLE`, `CONFLICTING` or `UNKNOWN`;
//!   `mergeStateStatus` is `DIRTY`, `UNKNOWN`, `BLOCKED`, `BEHIND`,
//!   `UNSTABLE`, `HAS_HOOKS` or `CLEAN`.
//! - `CheckRun.status` is `REQUESTED`, `QUEUED`, `IN_PROGRESS`, `COMPLETED`,
//!   `WAITING` or `PENDING`; its `conclusion` is `ACTION_REQUIRED`,
//!   `TIMED_OUT`, `CANCELLED`, `FAILURE`, `SUCCESS`, `NEUTRAL`, `SKIPPED`,
//!   `STARTUP_FAILURE` or `STALE`. `StatusContext.state` is `EXPECTED`,
//!   `ERROR`, `FAILURE`, `PENDING` or `SUCCESS`.
//! - An unknown pull request answers HTTP 200 with a null `pullRequest` and
//!   an error of type `NOT_FOUND`. A rejected token answers HTTP 401 with a
//!   REST-shaped body, `{"message": "Bad credentials", ...}`.
//! - `repository.pullRequests(headRefName:, baseRefName:, states:)` filters
//!   by branch; `headRepository` tells a fork's same-named branch apart.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::forge::{
    Branch, Capability, Check, CheckKind, CheckSet, CheckState, CommitId, Error, ErrorKind,
    Mergeable, PrState, PullRequest, Repo,
};
pub use crate::graphql::{Response, Transport};

/// GitHub's GraphQL endpoint.
pub const ENDPOINT: &str = "https://api.github.com/graphql";

/// The most items GitHub returns in one page.
pub const MAX_PAGE: u32 = 100;

/// A GitHub token: a fine-grained or classic personal access token, or the
/// output of `gh auth token`. It never shows in `Debug` output.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// The production transport: HTTPS to [`ENDPOINT`] with the token.
pub struct HttpTransport {
    agent: ureq::Agent,
    token: Token,
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTransport")
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl HttpTransport {
    pub fn new(token: Token) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            // A 401 or 403 carries a body worth reading.
            .http_status_as_error(false)
            // GitHub refuses requests without a user agent.
            .user_agent(concat!("owlshift/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            token,
        }
    }
}

impl Transport for HttpTransport {
    fn send(&self, request: &Value) -> Result<Response, String> {
        let mut response = self
            .agent
            .post(ENDPOINT)
            .header("Authorization", &format!("bearer {}", self.token.0))
            .send_json(request)
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        Ok(Response { status, body })
    }
}

/// A new pull request, with the body the runner wrote.
#[derive(Clone, Copy, Debug)]
pub struct NewPullRequest<'a> {
    pub head: &'a Branch,
    pub base: &'a Branch,
    pub title: &'a str,
    pub body: &'a str,
    /// Draft pull requests need a paid plan on a private repository.
    pub draft: bool,
}

/// One GitHub repository, as seen through one token. It can open a pull
/// request and read one; it cannot merge, close or approve.
pub struct GitHubForge {
    transport: Box<dyn Transport + Send + Sync>,
    repo: Repo,
    check_page: u32,
}

impl fmt::Debug for GitHubForge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubForge")
            .field("repo", &self.repo)
            .field("check_page", &self.check_page)
            .finish_non_exhaustive()
    }
}

impl GitHubForge {
    /// What this build does with GitHub. `PushBranch` is GitHub accepting
    /// branch pushes; the push itself is [`crate::forge::push`].
    pub const CAPABILITIES: &'static [Capability] = &[
        Capability::OpenPullRequest,
        Capability::ReadChecks,
        Capability::PushBranch,
        Capability::ReadMergeState,
    ];

    /// The adapter over HTTPS with this token.
    pub fn new(token: Token, repo: Repo) -> Self {
        Self::with_transport(HttpTransport::new(token), repo)
    }

    /// The adapter over another transport, such as a test replay.
    pub fn with_transport(transport: impl Transport + Send + Sync + 'static, repo: Repo) -> Self {
        Self {
            transport: Box::new(transport),
            repo,
            check_page: MAX_PAGE,
        }
    }

    /// Reads checks `size` at a time (1 to [`MAX_PAGE`], the default), so a
    /// recording can cover pagination on a pull request with few checks.
    pub fn with_check_page(mut self, size: u32) -> Self {
        self.check_page = size.clamp(1, MAX_PAGE);
        self
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    /// The open pull request from `head` of this repository into `base`, if
    /// there is one. A resumed delivery calls it before
    /// [`open_pull_request`](Self::open_pull_request), so it never opens a
    /// second one. A fork's branch of the same name is not this repository's.
    pub fn find_open_pull_request(
        &self,
        head: &Branch,
        base: &Branch,
    ) -> Result<Option<PullRequest>, Error> {
        let variables = json!({
            "owner": self.repo.owner(), "name": self.repo.name(),
            "head": head.as_str(), "base": base.as_str(),
        });
        let data: OpenData = self.call(OPEN_QUERY, variables)?;
        let found = data.repository.pull_requests;
        if found.total_count as usize > found.nodes.len() {
            return Err(invalid(format!(
                "more open pull requests from {head} into {base} than one page holds"
            )));
        }
        let ours: Vec<GhPullRequest> = found
            .nodes
            .into_iter()
            .filter(|pr| {
                pr.head_repository.as_ref().is_some_and(|r| {
                    r.name_with_owner
                        .eq_ignore_ascii_case(&self.repo.to_string())
                })
            })
            .collect();
        match <[GhPullRequest; 1]>::try_from(ours) {
            Ok([pr]) => pr.into_pull_request().map(Some),
            Err(ours) if ours.is_empty() => Ok(None),
            Err(ours) => Err(invalid(format!(
                "{} open pull requests from {head} into {base}",
                ours.len()
            ))),
        }
    }

    /// Opens a pull request with the runner's title and body, verbatim.
    pub fn open_pull_request(&self, new: NewPullRequest<'_>) -> Result<PullRequest, Error> {
        let variables = json!({ "owner": self.repo.owner(), "name": self.repo.name() });
        let data: RepositoryIdData = self.call(REPOSITORY_ID_QUERY, variables)?;
        let variables = json!({
            "input": {
                "repositoryId": data.repository.id,
                "headRefName": new.head.as_str(),
                "baseRefName": new.base.as_str(),
                "title": new.title,
                "body": new.body,
                "draft": new.draft,
            }
        });
        let data: CreateData = self.call(CREATE_MUTATION, variables)?;
        match data.create_pull_request.and_then(|c| c.pull_request) {
            Some(pr) => pr.into_pull_request(),
            None => Err(invalid(format!(
                "GitHub did not open a pull request from {}",
                new.head
            ))),
        }
    }

    /// Every check of pull request `number`, which must be at
    /// `expected_head`, the commit the runner pushed; with its state and
    /// merge state. Every page is read; the list is refused rather than
    /// returned partial when the pages do not advance, the count differs
    /// from GitHub's total, or the head moves while reading.
    pub fn checks(&self, number: u64, expected_head: &CommitId) -> Result<CheckSet, Error> {
        let mut checks = Vec::new();
        let mut after: Option<String> = None;
        let mut total: Option<u64> = None;
        loop {
            let variables = json!({
                "owner": self.repo.owner(), "name": self.repo.name(), "number": number,
                "first": self.check_page, "after": after,
            });
            let data: ChecksData = self.call(CHECKS_QUERY, variables)?;
            let pr = data.repository.pull_request.ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("no pull request #{number} in {}", self.repo),
                )
            })?;
            let head = pr.head_ref_oid.as_str();
            if head != expected_head.as_str() {
                return Err(Error::new(
                    ErrorKind::HeadMoved,
                    format!(
                        "pull request #{number} is at {head}, not at the pushed commit {expected_head}"
                    ),
                ));
            }
            let commit = match <[CommitNode; 1]>::try_from(pr.commits.nodes) {
                Ok([node]) if node.commit.oid == head => node.commit,
                _ => {
                    return Err(Error::new(
                        ErrorKind::HeadMoved,
                        format!("pull request #{number} changed while its checks were read"),
                    ));
                }
            };
            let (nodes, page_info) = match commit.status_check_rollup {
                Some(rollup) => {
                    let contexts = rollup.contexts;
                    if total.is_some_and(|t| t != contexts.total_count) {
                        return Err(invalid(format!(
                            "the checks of pull request #{number} changed while they were read"
                        )));
                    }
                    total = Some(contexts.total_count);
                    (contexts.nodes, contexts.page_info)
                }
                None if after.is_none() => (Vec::new(), PageInfo::last()),
                None => {
                    return Err(invalid(format!(
                        "the checks of pull request #{number} vanished while they were read"
                    )));
                }
            };
            checks.extend(nodes.into_iter().map(Context::into_check));
            if !page_info.has_next_page {
                let expected = total.unwrap_or(0);
                if checks.len() as u64 != expected {
                    return Err(invalid(format!(
                        "read {} checks of pull request #{number}, GitHub counts {expected}",
                        checks.len()
                    )));
                }
                return Ok(CheckSet {
                    pull_request: number,
                    head: expected_head.clone(),
                    state: pr_state(&pr.state)?,
                    mergeable: mergeable(&pr.mergeable),
                    merge_state: pr.merge_state_status,
                    checks,
                });
            }
            match page_info.end_cursor {
                Some(cursor) if after.as_ref() != Some(&cursor) => after = Some(cursor),
                _ => return Err(invalid("the check pages do not advance")),
            }
        }
    }

    /// Sends one request and returns its `data`, decoded.
    fn call<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T, Error> {
        let request = json!({ "query": query, "variables": variables });
        let response = self
            .transport
            .send(&request)
            .map_err(|e| invalid(format!("GitHub: {e}")))?;
        decode(&response)
    }
}

const OPEN_QUERY: &str = "query OpenPullRequests($owner: String!, $name: String!, \
    $head: String!, $base: String!) { repository(owner: $owner, name: $name) { \
    pullRequests(headRefName: $head, baseRefName: $base, states: [OPEN], first: 10) { \
    totalCount nodes { number url state headRefOid headRepository { nameWithOwner } } } } }";

const REPOSITORY_ID_QUERY: &str = "query RepositoryId($owner: String!, $name: String!) { \
    repository(owner: $owner, name: $name) { id } }";

const CREATE_MUTATION: &str = "mutation OpenPullRequest($input: CreatePullRequestInput!) { \
    createPullRequest(input: $input) { pullRequest { number url state headRefOid } } }";

const CHECKS_QUERY: &str = "query Checks($owner: String!, $name: String!, $number: Int!, \
    $first: Int!, $after: String) { repository(owner: $owner, name: $name) { \
    pullRequest(number: $number) { state headRefOid mergeable mergeStateStatus \
    commits(last: 1) { nodes { commit { oid statusCheckRollup { \
    contexts(first: $first, after: $after) { totalCount pageInfo { hasNextPage endCursor } \
    nodes { __typename \
    ... on CheckRun { name status conclusion isRequired(pullRequestNumber: $number) detailsUrl \
    checkSuite { app { slug } workflowRun { workflow { name } } } } \
    ... on StatusContext { context state isRequired(pullRequestNumber: $number) targetUrl } \
    } } } } } } } } }";

/// Reads a GraphQL answer: a rejected token first, then GraphQL errors, then
/// `data`.
fn decode<T: DeserializeOwned>(response: &Response) -> Result<T, Error> {
    let envelope = serde_json::from_str::<Envelope>(&response.body).ok();
    if response.status == 401 {
        let message = envelope.and_then(|e| e.message).unwrap_or_default();
        return Err(Error::new(
            ErrorKind::Unauthorized,
            format!("GitHub refused the token: HTTP 401 {message}"),
        ));
    }
    let Some(envelope) = envelope else {
        return Err(invalid(format!(
            "GitHub answered HTTP {} with a body that is not JSON",
            response.status
        )));
    };
    if let Some(error) = envelope.errors.into_iter().next() {
        let kind = match error.kind.as_deref() {
            Some("NOT_FOUND") => ErrorKind::NotFound,
            _ => ErrorKind::Other,
        };
        let code = error.kind.as_deref().unwrap_or("no type");
        return Err(Error::new(
            kind,
            format!("GitHub: {code}: {}", error.message),
        ));
    }
    if response.status != 200 {
        let message = envelope.message.unwrap_or_default();
        return Err(invalid(format!(
            "GitHub answered HTTP {} {message}",
            response.status
        )));
    }
    match envelope.data {
        Some(data) if !data.is_null() => serde_json::from_value(data)
            .map_err(|e| invalid(format!("unexpected answer from GitHub: {e}"))),
        _ => Err(invalid("GitHub answered without data")),
    }
}

fn pr_state(value: &str) -> Result<PrState, Error> {
    match value {
        "OPEN" => Ok(PrState::Open),
        "CLOSED" => Ok(PrState::Closed),
        "MERGED" => Ok(PrState::Merged),
        other => Err(invalid(format!("unknown pull request state {other}"))),
    }
}

fn mergeable(value: &str) -> Mergeable {
    match value {
        "MERGEABLE" => Mergeable::Mergeable,
        "CONFLICTING" => Mergeable::Conflicting,
        _ => Mergeable::Unknown,
    }
}

/// A check run: pending until completed, then by its conclusion. A
/// completed run without a conclusion, or with one the adapter does not
/// know, is failed.
fn check_run_state(status: &str, conclusion: Option<&str>) -> CheckState {
    match (status, conclusion) {
        ("COMPLETED", Some("SUCCESS" | "NEUTRAL" | "SKIPPED")) => CheckState::Passed,
        ("COMPLETED", _) => CheckState::Failed,
        _ => CheckState::Pending,
    }
}

/// A commit status: `EXPECTED` is a required status not reported yet.
fn status_state(state: &str) -> CheckState {
    match state {
        "SUCCESS" => CheckState::Passed,
        "PENDING" | "EXPECTED" => CheckState::Pending,
        _ => CheckState::Failed,
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::Other, message)
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
    /// The REST-shaped error of a refused or rate-limited request.
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
    #[serde(default, rename = "type")]
    kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPullRequest {
    number: u64,
    url: String,
    state: String,
    head_ref_oid: String,
    #[serde(default)]
    head_repository: Option<NameWithOwner>,
}

impl GhPullRequest {
    fn into_pull_request(self) -> Result<PullRequest, Error> {
        Ok(PullRequest {
            number: self.number,
            url: self.url,
            head: CommitId::new(self.head_ref_oid)?,
            state: pr_state(&self.state)?,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NameWithOwner {
    name_with_owner: String,
}

#[derive(Deserialize)]
struct OpenData {
    repository: OpenRepository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenRepository {
    pull_requests: Counted<GhPullRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Counted<T> {
    total_count: u64,
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct RepositoryIdData {
    repository: RepositoryId,
}

#[derive(Deserialize)]
struct RepositoryId {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateData {
    create_pull_request: Option<Created>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Created {
    pull_request: Option<GhPullRequest>,
}

#[derive(Deserialize)]
struct ChecksData {
    repository: ChecksRepository,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChecksRepository {
    pull_request: Option<ChecksPullRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChecksPullRequest {
    state: String,
    head_ref_oid: String,
    mergeable: String,
    merge_state_status: String,
    commits: Nodes<CommitNode>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Commit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    oid: String,
    status_check_rollup: Option<Rollup>,
}

#[derive(Deserialize)]
struct Rollup {
    contexts: Contexts,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Contexts {
    total_count: u64,
    page_info: PageInfo,
    nodes: Vec<Context>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    #[serde(default)]
    end_cursor: Option<String>,
}

impl PageInfo {
    fn last() -> Self {
        Self {
            has_next_page: false,
            end_cursor: None,
        }
    }
}

/// A rollup context. An unknown `__typename` fails the decoding: a kind of
/// check the adapter cannot read is never dropped silently.
#[derive(Deserialize)]
#[serde(tag = "__typename")]
enum Context {
    #[serde(rename_all = "camelCase")]
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
        #[serde(default)]
        is_required: Option<bool>,
        #[serde(default)]
        details_url: Option<String>,
        #[serde(default)]
        check_suite: Option<CheckSuite>,
    },
    #[serde(rename_all = "camelCase")]
    StatusContext {
        context: String,
        state: String,
        #[serde(default)]
        is_required: Option<bool>,
        #[serde(default)]
        target_url: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckSuite {
    #[serde(default)]
    app: Option<Slug>,
    #[serde(default)]
    workflow_run: Option<WorkflowRun>,
}

#[derive(Deserialize)]
struct Slug {
    slug: String,
}

#[derive(Deserialize)]
struct WorkflowRun {
    workflow: Named,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

impl Context {
    fn into_check(self) -> Check {
        match self {
            Self::CheckRun {
                name,
                status,
                conclusion,
                is_required,
                details_url,
                check_suite,
            } => {
                let state = check_run_state(&status, conclusion.as_deref());
                let (app, workflow) = match check_suite {
                    Some(suite) => (
                        suite.app.map(|a| a.slug),
                        suite.workflow_run.map(|w| w.workflow.name),
                    ),
                    None => (None, None),
                };
                Check {
                    kind: CheckKind::CheckRun { app, workflow },
                    name,
                    state,
                    raw: match (state, conclusion) {
                        (CheckState::Pending, _) | (_, None) => status,
                        (_, Some(conclusion)) => conclusion,
                    },
                    required: is_required.unwrap_or(false),
                    url: details_url,
                }
            }
            Self::StatusContext {
                context,
                state,
                is_required,
                target_url,
            } => Check {
                kind: CheckKind::Status,
                name: context,
                state: status_state(&state),
                raw: state,
                required: is_required.unwrap_or(false),
                url: target_url,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const HEAD: &str = "36f0557116ea73f9b0805980e83b360e23b99940";
    const OTHER: &str = "1111111111111111111111111111111111111111";

    fn answer(status: u16, body: &str) -> Response {
        Response {
            status,
            body: body.to_owned(),
        }
    }

    fn error(status: u16, body: &str) -> Error {
        decode::<Value>(&answer(status, body)).unwrap_err()
    }

    #[test]
    fn errors_are_classified_by_status_and_type() {
        let bad = error(401, r#"{"message":"Bad credentials","status":"401"}"#);
        assert_eq!(bad.kind, ErrorKind::Unauthorized);
        assert!(bad.message.contains("Bad credentials"), "{bad}");
        assert_eq!(error(401, "nope").kind, ErrorKind::Unauthorized);
        let missing = r#"{"data":{"repository":null},"errors":[{"type":"NOT_FOUND","message":"Could not resolve"}]}"#;
        assert_eq!(error(200, missing).kind, ErrorKind::NotFound);
        let unprocessable = r#"{"data":{"createPullRequest":null},"errors":[{"type":"UNPROCESSABLE","message":"A pull request already exists"}]}"#;
        let e = error(200, unprocessable);
        assert_eq!(e.kind, ErrorKind::Other);
        assert_eq!(
            e.message,
            "GitHub: UNPROCESSABLE: A pull request already exists"
        );
        let limited = error(403, r#"{"message":"API rate limit exceeded"}"#);
        assert_eq!(limited.kind, ErrorKind::Other);
        assert!(
            limited.message.contains("403 API rate limit exceeded"),
            "{limited}"
        );
        assert_eq!(error(502, "<html>").kind, ErrorKind::Other);
        assert_eq!(error(200, r#"{"data":null}"#).kind, ErrorKind::Other);
    }

    #[test]
    fn check_runs_and_statuses_classify_conservatively() {
        use CheckState::*;
        for (status, conclusion, expected) in [
            ("COMPLETED", Some("SUCCESS"), Passed),
            ("COMPLETED", Some("NEUTRAL"), Passed),
            ("COMPLETED", Some("SKIPPED"), Passed),
            ("COMPLETED", Some("FAILURE"), Failed),
            ("COMPLETED", Some("CANCELLED"), Failed),
            ("COMPLETED", Some("TIMED_OUT"), Failed),
            ("COMPLETED", Some("ACTION_REQUIRED"), Failed),
            ("COMPLETED", Some("STARTUP_FAILURE"), Failed),
            ("COMPLETED", Some("STALE"), Failed),
            ("COMPLETED", Some("SOMETHING_NEW"), Failed),
            ("COMPLETED", None, Failed),
            ("QUEUED", None, Pending),
            ("IN_PROGRESS", None, Pending),
            ("WAITING", None, Pending),
            ("REQUESTED", None, Pending),
            ("PENDING", None, Pending),
        ] {
            assert_eq!(
                check_run_state(status, conclusion),
                expected,
                "{status} {conclusion:?}"
            );
        }
        for (state, expected) in [
            ("SUCCESS", Passed),
            ("PENDING", Pending),
            ("EXPECTED", Pending),
            ("FAILURE", Failed),
            ("ERROR", Failed),
            ("SOMETHING_NEW", Failed),
        ] {
            assert_eq!(status_state(state), expected, "{state}");
        }
        assert_eq!(mergeable("SOMETHING_NEW"), Mergeable::Unknown);
    }

    /// Answers with prepared bodies, in order, and keeps the requests.
    struct Canned {
        bodies: Mutex<Vec<String>>,
        requests: Mutex<Vec<Value>>,
    }

    impl Transport for std::sync::Arc<Canned> {
        fn send(&self, request: &Value) -> Result<Response, String> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(answer(200, &self.bodies.lock().unwrap().remove(0)))
        }
    }

    fn forge(bodies: Vec<String>) -> (GitHubForge, std::sync::Arc<Canned>) {
        let canned = std::sync::Arc::new(Canned {
            bodies: Mutex::new(bodies),
            requests: Mutex::default(),
        });
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        (GitHubForge::with_transport(canned.clone(), repo), canned)
    }

    /// A checks page: the head, the rollup total, the contexts, the cursor.
    fn page(head: &str, commit: &str, total: u64, nodes: &str, next: Option<&str>) -> String {
        let info = match next {
            Some(cursor) => format!(r#"{{"hasNextPage":true,"endCursor":"{cursor}"}}"#),
            None => r#"{"hasNextPage":false,"endCursor":null}"#.to_owned(),
        };
        format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{"state":"OPEN","headRefOid":"{head}",
            "mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","commits":{{"nodes":[{{"commit":{{
            "oid":"{commit}","statusCheckRollup":{{"contexts":{{"totalCount":{total},
            "pageInfo":{info},"nodes":[{nodes}]}}}}}}}}]}}}}}}}}}}"#
        )
    }

    const RUN: &str = r#"{"__typename":"CheckRun","name":"ci","status":"COMPLETED","conclusion":"SUCCESS","isRequired":true,"detailsUrl":"u","checkSuite":{"app":{"slug":"github-actions"},"workflowRun":{"workflow":{"name":"CI"}}}}"#;
    const STATUS: &str = r#"{"__typename":"StatusContext","context":"deploy","state":"PENDING","isRequired":false,"targetUrl":null}"#;

    fn head() -> CommitId {
        CommitId::new(HEAD).unwrap()
    }

    #[test]
    fn checks_are_read_across_pages_with_both_kinds() {
        let (forge, canned) = forge(vec![
            page(HEAD, HEAD, 2, RUN, Some("c1")),
            page(HEAD, HEAD, 2, STATUS, None),
        ]);
        let set = forge.with_check_page(1).checks(7, &head()).unwrap();
        let requests = canned.requests.lock().unwrap();
        assert_eq!(requests[0]["variables"]["after"], Value::Null);
        assert_eq!(requests[1]["variables"]["after"], "c1");
        assert_eq!(requests[1]["variables"]["first"], 1);
        assert_eq!(set.checks.len(), 2);
        assert_eq!(
            set.checks[0].kind,
            CheckKind::CheckRun {
                app: Some("github-actions".into()),
                workflow: Some("CI".into())
            }
        );
        assert!(set.checks[0].required);
        assert_eq!(set.checks[1].kind, CheckKind::Status);
        assert_eq!(set.checks[1].raw, "PENDING");
        assert_eq!(set.verdict(), crate::forge::Verdict::Pending);
    }

    #[test]
    fn a_partial_or_moving_check_set_is_refused() {
        let check = |bodies: Vec<String>| forge(bodies).0.with_check_page(1).checks(7, &head());
        // The head is not the pushed commit.
        let e = check(vec![page(OTHER, OTHER, 1, RUN, None)]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::HeadMoved, "{e}");
        // The last commit is not the head: a push landed between the two.
        let e = check(vec![page(HEAD, OTHER, 1, RUN, None)]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::HeadMoved, "{e}");
        // The head moves between pages.
        let e = check(vec![
            page(HEAD, HEAD, 2, RUN, Some("c1")),
            page(OTHER, OTHER, 2, RUN, None),
        ])
        .unwrap_err();
        assert_eq!(e.kind, ErrorKind::HeadMoved, "{e}");
        // Fewer checks than GitHub counts.
        assert!(check(vec![page(HEAD, HEAD, 3, RUN, None)]).is_err());
        // The total changes between pages.
        assert!(
            check(vec![
                page(HEAD, HEAD, 2, RUN, Some("c1")),
                page(HEAD, HEAD, 3, RUN, None)
            ])
            .is_err()
        );
        // The cursor does not advance.
        assert!(
            check(vec![
                page(HEAD, HEAD, 3, RUN, Some("c1")),
                page(HEAD, HEAD, 3, RUN, Some("c1"))
            ])
            .is_err()
        );
        // A kind of check the adapter cannot read.
        let unknown = r#"{"__typename":"SomethingNew","name":"x"}"#;
        assert!(check(vec![page(HEAD, HEAD, 1, unknown, None)]).is_err());
    }

    #[test]
    fn a_commit_without_checks_has_an_empty_set() {
        let body = format!(
            r#"{{"data":{{"repository":{{"pullRequest":{{"state":"OPEN","headRefOid":"{HEAD}",
            "mergeable":"UNKNOWN","mergeStateStatus":"UNKNOWN","commits":{{"nodes":[{{"commit":{{
            "oid":"{HEAD}","statusCheckRollup":null}}}}]}}}}}}}}}}"#
        );
        let set = forge(vec![body]).0.checks(7, &head()).unwrap();
        assert!(set.checks.is_empty());
        assert_eq!(set.mergeable, Mergeable::Unknown);
        assert_eq!(set.verdict(), crate::forge::Verdict::Pending);
    }

    fn open_page(nodes: &str, total: u64) -> String {
        format!(
            r#"{{"data":{{"repository":{{"pullRequests":{{"totalCount":{total},"nodes":[{nodes}]}}}}}}}}"#
        )
    }

    fn pr_node(number: u64, repo: &str) -> String {
        format!(
            r#"{{"number":{number},"url":"https://github.com/{repo}/pull/{number}","state":"OPEN",
            "headRefOid":"{HEAD}","headRepository":{{"nameWithOwner":"{repo}"}}}}"#
        )
    }

    #[test]
    fn only_this_repositorys_single_open_pull_request_is_found() {
        let head = Branch::new("owl-17").unwrap();
        let base = Branch::new("main").unwrap();
        let find = |body: String| forge(vec![body]).0.find_open_pull_request(&head, &base);

        let fork = pr_node(3, "someone/owlshift");
        assert_eq!(find(open_page(&fork, 1)).unwrap(), None);
        let found = find(open_page(
            &format!("{fork},{}", pr_node(4, "pitchopp/owlshift")),
            2,
        ))
        .unwrap()
        .unwrap();
        assert_eq!(found.number, 4);
        assert_eq!(found.state, PrState::Open);
        let deleted_fork = r#"{"number":5,"url":"u","state":"OPEN","headRefOid":"36f0557116ea73f9b0805980e83b360e23b99940","headRepository":null}"#;
        assert_eq!(find(open_page(deleted_fork, 1)).unwrap(), None);

        let two = format!(
            "{},{}",
            pr_node(4, "pitchopp/owlshift"),
            pr_node(5, "pitchopp/owlshift")
        );
        assert!(find(open_page(&two, 2)).is_err());
        assert!(find(open_page(&pr_node(4, "pitchopp/owlshift"), 11)).is_err());
    }

    #[test]
    fn a_pull_request_github_did_not_open_is_an_error() {
        let head = Branch::new("owl-17").unwrap();
        let base = Branch::new("main").unwrap();
        let new = NewPullRequest {
            head: &head,
            base: &base,
            title: "t",
            body: "b",
            draft: false,
        };
        let (forge, _) = forge(vec![
            r#"{"data":{"repository":{"id":"R_1"}}}"#.to_owned(),
            r#"{"data":{"createPullRequest":null}}"#.to_owned(),
        ]);
        assert!(forge.open_pull_request(new).is_err());
    }

    #[test]
    fn the_token_never_shows_in_debug_output() {
        let token = Token::new("ghp_do_not_print");
        let transport = HttpTransport::new(token.clone());
        let forge = GitHubForge::new(token.clone(), Repo::parse("o/n").unwrap());
        let shown = format!("{token:?} {transport:?} {forge:?}");
        assert!(!shown.contains("do_not_print"), "{shown}");
    }
}
