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
use std::io::{self, BufRead, BufReader, Read};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use jiff::Timestamp;
use owlshift_contracts::brief::MAX_EXCERPT_BYTES;

use crate::forge::{
    BaseChecks, Branch, Capability, Check, CheckKind, CheckSet, CheckState, CommitId,
    DefaultBranch, Error, ErrorKind, InlineComment, Mergeable, PrState, PullRequest,
    PullRequestRead, RateLimit, Repo, Review, ReviewsRead,
};
pub use crate::graphql::{RateHeaders, Response, RestAnswer, Transport};

/// GitHub's GraphQL endpoint.
pub const ENDPOINT: &str = "https://api.github.com/graphql";

/// GitHub's REST API, which the job logs and another app's output are read
/// from (OWL-112).
pub const REST_API: &str = "https://api.github.com/";

/// The most bytes of a job's log read: past it, the excerpt is the end of
/// what was read.
pub const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

/// The most bytes of a REST answer read whole, such as a check run's.
const MAX_REST_BYTES: u64 = 1024 * 1024;

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
        Self {
            agent: agent(Duration::from_secs(30)),
            token,
        }
    }
}

/// An agent that follows no redirect: [`HttpTransport::get`] follows the
/// one a log answers itself, without the token.
fn agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        // A 401 or 403 carries a body worth reading.
        .http_status_as_error(false)
        .max_redirects(0)
        // GitHub refuses requests without a user agent.
        .user_agent(concat!("owlshift/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

/// Where a REST answer redirects to, when it is a redirect: an HTTPS URL
/// with no credential of its own, which the runner reads without the token
/// (architecture section 5, "The fix run"). A log answers with a signed
/// storage URL.
pub fn redirect_target(status: u16, location: Option<&str>) -> Result<Option<String>, String> {
    if !(300..400).contains(&status) {
        return Ok(None);
    }
    let Some(location) = location else {
        return Err(format!("GitHub answered HTTP {status} without a location"));
    };
    let Some(rest) = location.strip_prefix("https://") else {
        return Err(format!(
            "GitHub redirected to {}, which is not HTTPS",
            redact(location)
        ));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(format!(
            "GitHub redirected to {}, which names no host or carries a credential",
            redact(location)
        ));
    }
    Ok(Some(location.to_owned()))
}

/// `text` with the query of every URL in it removed: a signed storage URL's
/// query is its credential.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        // A URL runs to the next space or quote.
        let end = rest[at..]
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
            .map_or(rest.len(), |end| at + end);
        match rest[at..end].find('?') {
            Some(query) => {
                out.push_str(&rest[..at + query]);
                out.push_str("?<query removed>");
            }
            None => out.push_str(&rest[..end]),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
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
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_owned())
        };
        // The rate-limit headers alone (OWL-209).
        let rate = RateHeaders {
            remaining: header("x-ratelimit-remaining").and_then(|v| v.parse().ok()),
            reset: header("x-ratelimit-reset").and_then(|v| v.parse().ok()),
            retry_after: header("retry-after").and_then(|v| v.parse().ok()),
        };
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        Ok(Response { status, body, rate })
    }

    fn get(&self, path: &str) -> Result<RestAnswer, String> {
        let answer = self
            .agent
            .get(&format!("{REST_API}{path}"))
            .header("Authorization", &format!("bearer {}", self.token.0))
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| redact(&e.to_string()))?;
        let status = answer.status().as_u16();
        let header = |name: &str| {
            answer
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_owned())
        };
        let rate = RateHeaders {
            remaining: header("x-ratelimit-remaining").and_then(|v| v.parse().ok()),
            reset: header("x-ratelimit-reset").and_then(|v| v.parse().ok()),
            retry_after: header("retry-after").and_then(|v| v.parse().ok()),
        };
        let answer = match redirect_target(status, header("location").as_deref())? {
            // A fresh request through an agent that never held the token.
            Some(target) => agent(Duration::from_secs(120))
                .get(&target)
                .call()
                .map_err(|e| redact(&e.to_string()))?,
            None => answer,
        };
        let status = answer.status().as_u16();
        let body = answer
            .into_body()
            .into_with_config()
            .limit(u64::MAX)
            .reader();
        Ok(RestAnswer {
            status,
            rate,
            body: Box::new(body),
        })
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
        Capability::ReadReviewComments,
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
        let data: CreateData = self.call(CREATE_MUTATION, variables).map_err(refs_hint)?;
        match data.create_pull_request.and_then(|c| c.pull_request) {
            Some(pr) => pr.into_pull_request(),
            None => Err(invalid(format!(
                "GitHub did not open a pull request from {}",
                new.head
            ))),
        }
    }

    /// Whether the token reads the repository's default branch (OWL-334),
    /// in one read-only query: GitHub opens a pull request only on refs the
    /// token can read, which a fine-grained token gets with contents (read).
    /// A refusal is [`DefaultBranch::Forbidden`]: a GraphQL error of type
    /// `FORBIDDEN`, or HTTP 403 that is no rate limit. A repository the
    /// token cannot see is [`ErrorKind::NotFound`].
    pub fn default_branch(&self) -> Result<DefaultBranch, Error> {
        let variables = json!({ "owner": self.repo.owner(), "name": self.repo.name() });
        let response = self.send(DEFAULT_BRANCH_QUERY, variables)?;
        if let Some(message) = forbidden(&response) {
            return Ok(DefaultBranch::Forbidden(message));
        }
        let data: DefaultBranchData = decode(&response)?;
        let Some(repository) = data.repository else {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("GitHub shows this token no repository {}", self.repo),
            ));
        };
        let Some(found) = repository.default_branch_ref else {
            return Ok(DefaultBranch::Empty);
        };
        // A ref read whole: its name and the commit it points at.
        CommitId::new(found.target.oid)?;
        Branch::new(found.name).map(DefaultBranch::Readable)
    }

    /// Every check of pull request `number`, which must be at
    /// `expected_head`, the commit the runner pushed; with its state and
    /// merge state. Every page is read; the list is refused rather than
    /// returned partial when the pages do not advance, the count differs
    /// from GitHub's total, or the head moves while reading. A head that is
    /// not the expected one is [`ErrorKind::HeadMoved`] with the head read.
    pub fn checks(&self, number: u64, expected_head: &CommitId) -> Result<CheckSet, Error> {
        self.read_checks(CHECKS_QUERY, number, Some(expected_head))
            .map(|(set, _)| set)
    }

    /// Watch's read of pull request `number` (OWL-209): its state, its head
    /// whoever pushed it, its mergeability and, while it is open, every check
    /// of that head, as complete as [`checks`](Self::checks) reads them;
    /// with the query's cost. A pull request that is not open lists no check.
    pub fn follow(&self, number: u64) -> Result<PullRequestRead, Error> {
        let (checks, rate) = self.read_checks(FOLLOW_QUERY, number, None)?;
        Ok(PullRequestRead { checks, rate })
    }

    /// The reviews submitted on pull request `number` (OWL-307), in one
    /// query: the newest 100, oldest first, each with its first 100 inline
    /// comments, and the query's cost. A pending review, which only its
    /// author sees, is never asked for (checked live on 2026-10-16: GitHub
    /// shows the token's owner their own pending review unless the states
    /// are named); one without a submission time is dropped likewise.
    pub fn reviews(&self, number: u64) -> Result<ReviewsRead, Error> {
        let variables = json!({
            "owner": self.repo.owner(), "name": self.repo.name(), "number": number,
        });
        let data: ReviewsData = self.call(REVIEWS_QUERY, variables)?;
        let mut rate = Rates::default();
        rate.add(data.rate_limit);
        let pr = data.repository.pull_request.ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!("no pull request #{number} in {}", self.repo),
            )
        })?;
        let mut reviews = Vec::with_capacity(pr.reviews.nodes.len());
        for node in pr.reviews.nodes {
            let Some(submitted_at) = node.submitted_at else {
                continue;
            };
            let id = node.full_database_id.parse::<u64>().map_err(|_| {
                invalid(format!(
                    "unexpected review id {:?} from GitHub",
                    node.full_database_id
                ))
            })?;
            reviews.push(Review {
                id,
                author: node.author.map(|author| author.login),
                state: node.state,
                submitted_at,
                body: node.body,
                commit: node
                    .commit
                    .and_then(|commit| CommitId::new(commit.oid).ok()),
                comments: node
                    .comments
                    .nodes
                    .into_iter()
                    .map(|comment| InlineComment {
                        path: comment.path,
                        line: comment.line,
                        body: comment.body,
                    })
                    .collect(),
            });
        }
        Ok(ReviewsRead {
            reviews,
            rate: rate.get(),
        })
    }

    /// The checks of `base`'s head and, when given, of `at_push`, each read
    /// to the last page, in one query while each fits a page (OWL-209).
    /// Checks of a commit are not tied to a pull request, so none is
    /// required. `at_push` that GitHub does not know as a commit has none.
    pub fn base_checks(
        &self,
        base: &Branch,
        at_push: Option<&CommitId>,
    ) -> Result<BaseChecks, Error> {
        let variables = json!({
            "owner": self.repo.owner(), "name": self.repo.name(),
            "head": format!("refs/heads/{base}"),
            // Without `base_at_push`, the base head again, its answer unused:
            // one query shape, and no directive.
            "atPush": at_push.map_or_else(|| format!("refs/heads/{base}"), |c| c.to_string()),
            "first": self.check_page,
        });
        let data: BaseData = self.call(BASE_CHECKS_QUERY, variables)?;
        let mut rate = Rates::default();
        rate.add(data.rate_limit);
        let head = data
            .repository
            .head
            .and_then(|object| object.into_commit())
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("no branch {base} in {}", self.repo),
                )
            })?;
        let (head, rollup) = head;
        let at_head = self.every_context(&head, rollup, &mut rate)?;
        let at_push = match data.repository.at_push.and_then(|o| o.into_commit()) {
            Some((oid, rollup)) if at_push == Some(&oid) => {
                Some(self.every_context(&oid, rollup, &mut rate)?)
            }
            _ => None,
        };
        Ok(BaseChecks {
            head,
            at_head,
            at_push,
            rate: rate.get(),
        })
    }

    /// The excerpt of a GitHub Actions job's log (OWL-112): the job is the
    /// check run of the same id. The log is streamed, never held whole, and
    /// read to [`MAX_LOG_BYTES`] at most ([`log_excerpt`]). The redirect it
    /// answers is followed without the token. A refused token is
    /// [`ErrorKind::Unauthorized`], HTTP 401 or a 403 that is no rate limit,
    /// since a fine-grained token needs actions (read); a log GitHub no
    /// longer keeps is [`ErrorKind::NotFound`].
    pub fn job_log_excerpt(&self, job: u64) -> Result<String, Error> {
        let answer = self.rest(&format!(
            "repos/{}/{}/actions/jobs/{job}/logs",
            self.repo.owner(),
            self.repo.name()
        ))?;
        log_excerpt(answer.body.take(MAX_LOG_BYTES), MAX_EXCERPT_BYTES)
            .map_err(|e| invalid(format!("reading the log: {}", redact(&e.to_string()))))
    }

    /// Another app's check run's output (OWL-112): its title, summary and
    /// text, each when given, a blank line between them.
    pub fn check_run_output(&self, id: u64) -> Result<String, Error> {
        let answer = self.rest(&format!(
            "repos/{}/{}/check-runs/{id}",
            self.repo.owner(),
            self.repo.name()
        ))?;
        let mut body = String::new();
        answer
            .body
            .take(MAX_REST_BYTES)
            .read_to_string(&mut body)
            .map_err(|e| invalid(format!("reading the check run: {e}")))?;
        let run: RestCheckRun = serde_json::from_str(&body)
            .map_err(|e| invalid(format!("unexpected check run from GitHub: {e}")))?;
        let output = run.output.unwrap_or_default();
        Ok([output.title, output.summary, output.text]
            .into_iter()
            .flatten()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    /// One REST read: a successful answer, or the error its status says.
    fn rest(&self, path: &str) -> Result<RestAnswer, Error> {
        let mut answer = self
            .transport
            .get(path)
            .map_err(|e| invalid(format!("GitHub could not be reached: {}", redact(&e))))?;
        if (200..300).contains(&answer.status) {
            return Ok(answer);
        }
        let mut body = String::new();
        let _ = (&mut answer.body).take(4096).read_to_string(&mut body);
        let message = serde_json::from_str::<Envelope>(&body)
            .ok()
            .and_then(|e| e.message)
            .unwrap_or_default();
        let rate = answer.rate;
        let status = answer.status;
        Err(match status {
            403 | 429 if rate.remaining == Some(0) || rate.retry_after.is_some() => Error::new(
                ErrorKind::RateLimited,
                format!("GitHub's rate limit: HTTP {status} {message}"),
            )
            .with_reset(resets_at(rate, Timestamp::now())),
            401 | 403 => Error::new(
                ErrorKind::Unauthorized,
                format!("GitHub refused the token: HTTP {status} {message}"),
            ),
            404 | 410 => Error::new(
                ErrorKind::NotFound,
                format!("GitHub has no such {path}: HTTP {status} {message}"),
            ),
            _ => invalid(format!("GitHub answered HTTP {status} {message}")),
        })
    }

    /// The checks of a commit from the first page `rollup`, reading its
    /// other pages; refused when the pages do not advance or their count
    /// differs from GitHub's total.
    fn every_context(
        &self,
        oid: &CommitId,
        rollup: Option<Rollup>,
        rate: &mut Rates,
    ) -> Result<Vec<Check>, Error> {
        let Some(rollup) = rollup else {
            return Ok(Vec::new());
        };
        let total = rollup.contexts.total_count;
        let mut info = rollup.contexts.page_info;
        let mut checks: Vec<Check> = rollup
            .contexts
            .nodes
            .into_iter()
            .map(Context::into_check)
            .collect();
        while info.has_next_page {
            let cursor = info
                .end_cursor
                .clone()
                .ok_or_else(|| invalid("the check pages do not advance"))?;
            let variables = json!({
                "owner": self.repo.owner(), "name": self.repo.name(),
                "oid": oid.as_str(), "first": self.check_page, "after": cursor,
            });
            let data: CommitData = self.call(COMMIT_CHECKS_QUERY, variables)?;
            rate.add(data.rate_limit);
            let contexts = data
                .repository
                .object
                .and_then(|o| o.into_commit())
                .and_then(|(_, rollup)| rollup)
                .ok_or_else(|| {
                    invalid(format!("the checks of {oid} vanished while they were read"))
                })?
                .contexts;
            if contexts.total_count != total {
                return Err(invalid(format!(
                    "the checks of {oid} changed while they were read"
                )));
            }
            if contexts.page_info.has_next_page
                && contexts.page_info.end_cursor.as_ref() == Some(&cursor)
            {
                return Err(invalid("the check pages do not advance"));
            }
            checks.extend(contexts.nodes.into_iter().map(Context::into_check));
            info = contexts.page_info;
        }
        if checks.len() as u64 != total {
            return Err(invalid(format!(
                "read {} checks of {oid}, GitHub counts {total}",
                checks.len()
            )));
        }
        Ok(checks)
    }

    /// The check set of pull request `number` through `query`, at
    /// `expected` when given, else at the head the first page reads.
    fn read_checks(
        &self,
        query: &str,
        number: u64,
        expected: Option<&CommitId>,
    ) -> Result<(CheckSet, Option<RateLimit>), Error> {
        let mut checks = Vec::new();
        let mut after: Option<String> = None;
        let mut total: Option<u64> = None;
        let mut rate = Rates::default();
        let mut read_head: Option<CommitId> = None;
        loop {
            let variables = json!({
                "owner": self.repo.owner(), "name": self.repo.name(), "number": number,
                "first": self.check_page, "after": after,
            });
            let data: ChecksData = self.call(query, variables)?;
            rate.add(data.rate_limit);
            let pr = data.repository.pull_request.ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!("no pull request #{number} in {}", self.repo),
                )
            })?;
            let head = CommitId::new(pr.head_ref_oid.as_str())?;
            match (expected, &read_head) {
                (Some(expected), _) if head != *expected => {
                    return Err(Error::new(
                        ErrorKind::HeadMoved,
                        format!(
                            "pull request #{number} is at {head}, not at the pushed commit {expected}"
                        ),
                    )
                    .with_head(head));
                }
                (None, Some(first)) if head != *first => {
                    return Err(Error::new(
                        ErrorKind::HeadMoved,
                        format!("pull request #{number} changed while its checks were read"),
                    )
                    .with_head(head));
                }
                (None, None) => {
                    read_head = Some(head.clone());
                    let state = pr_state(&pr.state)?;
                    // Not open: watch stops following, whatever its checks.
                    if state != PrState::Open {
                        let set = CheckSet {
                            pull_request: number,
                            head,
                            state,
                            mergeable: mergeable(pr.mergeable.as_deref().unwrap_or_default()),
                            merge_state: pr.merge_state_status,
                            checks: Vec::new(),
                        };
                        return Ok((set, rate.get()));
                    }
                }
                _ => {}
            }
            let commit = match <[CommitNode; 1]>::try_from(pr.commits.nodes) {
                Ok([node]) if node.commit.oid == head.as_str() => node.commit,
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
                let expected_count = total.unwrap_or(0);
                if checks.len() as u64 != expected_count {
                    return Err(invalid(format!(
                        "read {} checks of pull request #{number}, GitHub counts {expected_count}",
                        checks.len()
                    )));
                }
                let set = CheckSet {
                    pull_request: number,
                    head,
                    state: pr_state(&pr.state)?,
                    mergeable: mergeable(pr.mergeable.as_deref().unwrap_or_default()),
                    merge_state: pr.merge_state_status,
                    checks,
                };
                return Ok((set, rate.get()));
            }
            match page_info.end_cursor {
                Some(cursor) if after.as_ref() != Some(&cursor) => after = Some(cursor),
                _ => return Err(invalid("the check pages do not advance")),
            }
        }
    }

    /// Sends one request and returns its `data`, decoded.
    fn call<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T, Error> {
        decode(&self.send(query, variables)?)
    }

    /// Sends one request and returns GitHub's answer, undecoded.
    fn send(&self, query: &str, variables: Value) -> Result<Response, Error> {
        let request = json!({ "query": query, "variables": variables });
        self.transport
            .send(&request)
            .map_err(|e| invalid(format!("GitHub: {e}")))
    }
}

const OPEN_QUERY: &str = "query OpenPullRequests($owner: String!, $name: String!, \
    $head: String!, $base: String!) { repository(owner: $owner, name: $name) { \
    pullRequests(headRefName: $head, baseRefName: $base, states: [OPEN], first: 10) { \
    totalCount nodes { number url state headRefOid headRepository { nameWithOwner } } } } }";

const REPOSITORY_ID_QUERY: &str = "query RepositoryId($owner: String!, $name: String!) { \
    repository(owner: $owner, name: $name) { id } }";

const DEFAULT_BRANCH_QUERY: &str = "query DefaultBranch($owner: String!, $name: String!) { \
    repository(owner: $owner, name: $name) { defaultBranchRef { name target { oid } } } }";

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

/// Watch's read of a pull request (OWL-209): the checks query with the
/// query's cost. Delivery's [`CHECKS_QUERY`] keeps the text its fixtures were
/// recorded live with.
const FOLLOW_QUERY: &str = "query Follow($owner: String!, $name: String!, $number: Int!, \
    $first: Int!, $after: String) { rateLimit { cost remaining resetAt } \
    repository(owner: $owner, name: $name) { \
    pullRequest(number: $number) { state headRefOid mergeable mergeStateStatus \
    commits(last: 1) { nodes { commit { oid statusCheckRollup { \
    contexts(first: $first, after: $after) { totalCount pageInfo { hasNextPage endCursor } \
    nodes { __typename \
    ... on CheckRun { databaseId name status conclusion \
    isRequired(pullRequestNumber: $number) detailsUrl \
    checkSuite { app { slug } workflowRun { workflow { name } } } } \
    ... on StatusContext { context state description isRequired(pullRequestNumber: $number) \
    targetUrl } \
    } } } } } } } } }";

/// The reviews of a pull request (OWL-307): every submitted state named, so
/// no pending review is read; the newest 100, each with its first 100
/// inline comments. `fullDatabaseId` is the REST id as a string: the ids
/// are past 2^31, the most GraphQL's `Int` holds.
const REVIEWS_QUERY: &str = "query Reviews($owner: String!, $name: String!, $number: Int!) { \
    rateLimit { cost remaining resetAt } repository(owner: $owner, name: $name) { \
    pullRequest(number: $number) { \
    reviews(last: 100, states: [COMMENTED, APPROVED, CHANGES_REQUESTED, DISMISSED]) { \
    nodes { fullDatabaseId author { login } state submittedAt body commit { oid } \
    comments(first: 100) { nodes { path line body } } } } } } }";

/// The checks of the base head and of `base_at_push` (OWL-209).
const BASE_CHECKS_QUERY: &str = "query BaseChecks($owner: String!, $name: String!, \
    $head: String!, $atPush: String!, $first: Int!) { \
    rateLimit { cost remaining resetAt } repository(owner: $owner, name: $name) { \
    head: object(expression: $head) { ... on Commit { oid statusCheckRollup { \
    contexts(first: $first) { totalCount pageInfo { hasNextPage endCursor } nodes { __typename \
    ... on CheckRun { name status conclusion detailsUrl \
    checkSuite { app { slug } workflowRun { workflow { name } } } } \
    ... on StatusContext { context state targetUrl } } } } } } \
    atPush: object(expression: $atPush) { ... on Commit { oid \
    statusCheckRollup { contexts(first: $first) { totalCount \
    pageInfo { hasNextPage endCursor } nodes { __typename \
    ... on CheckRun { name status conclusion detailsUrl \
    checkSuite { app { slug } workflowRun { workflow { name } } } } \
    ... on StatusContext { context state targetUrl } } } } } } } }";

/// The next page of a commit's checks (OWL-209).
const COMMIT_CHECKS_QUERY: &str = "query CommitChecks($owner: String!, $name: String!, \
    $oid: String!, $first: Int!, $after: String) { rateLimit { cost remaining resetAt } \
    repository(owner: $owner, name: $name) { object(expression: $oid) { ... on Commit { oid \
    statusCheckRollup { contexts(first: $first, after: $after) { totalCount \
    pageInfo { hasNextPage endCursor } nodes { __typename \
    ... on CheckRun { name status conclusion detailsUrl \
    checkSuite { app { slug } workflowRun { workflow { name } } } } \
    ... on StatusContext { context state targetUrl } } } } } } } }";

/// The excerpt of a GitHub Actions log read from `log` (OWL-112): each
/// line without its leading timestamp and its ANSI escapes, and the lines,
/// at most `max` bytes of them, ending with the first `##[error]` line, or
/// the log's last ones when it has none. Reading stops at that line. A line
/// longer than `max` keeps its end.
pub fn log_excerpt(log: impl Read, max: usize) -> io::Result<String> {
    let mut reader = BufReader::new(log);
    let mut kept: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut size = 0;
    let mut raw = Vec::new();
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        let text = String::from_utf8_lossy(&raw);
        let line = clean_log_line(text.trim_end_matches(['\n', '\r']));
        let line = tail(&line, max.saturating_sub(1));
        let error = line.contains("##[error]");
        size += line.len() + 1;
        kept.push_back(line);
        while size > max {
            match kept.pop_front() {
                Some(front) => size -= front.len() + 1,
                None => break,
            }
        }
        if error {
            break;
        }
    }
    let mut excerpt = String::with_capacity(size);
    for line in kept {
        excerpt.push_str(&line);
        excerpt.push('\n');
    }
    Ok(excerpt)
}

/// The end of `text` within `max` bytes, cut on a character boundary.
fn tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

/// A log line without the timestamp GitHub Actions starts it with
/// (`2026-10-01T09:12:44.1234567Z `) and without ANSI escape sequences.
fn clean_log_line(line: &str) -> String {
    let line = match line.split_once(' ') {
        Some((stamp, rest)) if is_log_timestamp(stamp) => rest,
        _ => line,
    };
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI: ESC [ parameters, then one final byte from @ to ~; any other
        // escape drops the ESC and the next character.
        if chars.next_if_eq(&'[').is_some() {
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

fn is_log_timestamp(stamp: &str) -> bool {
    let bytes = stamp.as_bytes();
    bytes.len() >= 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes.ends_with(b"Z")
        && stamp
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'T' | b':' | b'.' | b'Z'))
}

/// A REST check run, its output alone.
#[derive(Deserialize)]
struct RestCheckRun {
    #[serde(default)]
    output: Option<RestOutput>,
}

#[derive(Default, Deserialize)]
struct RestOutput {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

/// GitHub's words when `createPullRequest` cannot read a ref of the pull
/// request, as answered on 2026-10-17 to a token without contents (read)
/// (OWL-334).
const REFS_UNREADABLE: &str = "not all refs are readable";

/// `error` with what it most likely means when it is GitHub refusing to
/// open a pull request on refs the token cannot read; any other unchanged.
fn refs_hint(mut error: Error) -> Error {
    if error.message.starts_with("GitHub: UNPROCESSABLE: ")
        && error.message.contains(REFS_UNREADABLE)
    {
        error.message.push_str(
            " (the GitHub token probably lacks contents (read), which GitHub needs to read a \
             pull request's branches)",
        );
    }
    error
}

/// The refusal in `response` that is neither a rejected token nor a spent
/// budget: HTTP 403 without the rate-limit headers or a message naming a
/// rate limit (a secondary limit may set neither header), or a GraphQL
/// error of type `FORBIDDEN` among no `RATE_LIMITED` one. GitHub answered
/// such an error, "Resource not accessible by personal access token", to a
/// token without contents (read) on 2026-10-17 (OWL-334).
fn forbidden(response: &Response) -> Option<String> {
    let envelope = serde_json::from_str::<Envelope>(&response.body).ok();
    let rate = response.rate;
    if response.status == 403 {
        let message = envelope.and_then(|e| e.message).unwrap_or_default();
        let limited = rate.remaining == Some(0)
            || rate.retry_after.is_some()
            || message.to_ascii_lowercase().contains("rate limit");
        return (!limited).then(|| format!("GitHub answered HTTP 403 {message}"));
    }
    let errors = envelope?.errors;
    if response.status != 200
        || errors
            .iter()
            .any(|error| error.kind.as_deref() == Some("RATE_LIMITED"))
    {
        return None;
    }
    errors
        .into_iter()
        .find(|error| error.kind.as_deref() == Some("FORBIDDEN"))
        .map(|error| format!("GitHub: FORBIDDEN: {}", error.message))
}

/// Reads a GraphQL answer: a rejected token first, then a spent budget, then
/// GraphQL errors, then `data`.
fn decode<T: DeserializeOwned>(response: &Response) -> Result<T, Error> {
    let envelope = serde_json::from_str::<Envelope>(&response.body).ok();
    if response.status == 401 {
        let message = envelope.and_then(|e| e.message).unwrap_or_default();
        return Err(Error::new(
            ErrorKind::Unauthorized,
            format!("GitHub refused the token: HTTP 401 {message}"),
        ));
    }
    let rate = response.rate;
    let limited = |message: String| {
        Error::new(ErrorKind::RateLimited, message).with_reset(resets_at(rate, Timestamp::now()))
    };
    if matches!(response.status, 403 | 429)
        && (rate.remaining == Some(0) || rate.retry_after.is_some())
    {
        let message = envelope.and_then(|e| e.message).unwrap_or_default();
        return Err(limited(format!(
            "GitHub's rate limit: HTTP {} {message}",
            response.status
        )));
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
            Some("RATE_LIMITED") => {
                return Err(limited(format!("GitHub: RATE_LIMITED: {}", error.message)));
            }
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

/// When a spent budget resets: `Retry-After` from `now`, else
/// `x-ratelimit-reset`; `None` when the answer gives neither.
fn resets_at(rate: RateHeaders, now: Timestamp) -> Option<Timestamp> {
    match (rate.retry_after, rate.reset) {
        (Some(after), _) => now.checked_add(Duration::from_secs(after)).ok(),
        (None, Some(reset)) => Timestamp::from_second(reset).ok(),
        (None, None) => None,
    }
}

/// The cost of a read over its queries: the points summed, the budget and
/// reset of the last answer.
#[derive(Default)]
struct Rates(Option<RateLimit>);

impl Rates {
    fn add(&mut self, answer: Option<GhRateLimit>) {
        let Some(answer) = answer else {
            return;
        };
        let cost = self.0.map_or(0, |r| r.cost) + answer.cost;
        self.0 = Some(RateLimit {
            cost,
            remaining: answer.remaining,
            resets_at: answer.reset_at,
        });
    }

    fn get(&self) -> Option<RateLimit> {
        self.0
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
struct DefaultBranchData {
    repository: Option<DefaultBranchRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DefaultBranchRepository {
    default_branch_ref: Option<GhRef>,
}

/// A ref, with the commit it points at.
#[derive(Deserialize)]
struct GhRef {
    name: String,
    target: GhOid,
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
#[serde(rename_all = "camelCase")]
struct ChecksData {
    repository: ChecksRepository,
    #[serde(default)]
    rate_limit: Option<GhRateLimit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewsData {
    repository: ReviewsRepository,
    #[serde(default)]
    rate_limit: Option<GhRateLimit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewsRepository {
    pull_request: Option<ReviewsPullRequest>,
}

#[derive(Deserialize)]
struct ReviewsPullRequest {
    reviews: Nodes<GhReview>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhReview {
    full_database_id: String,
    #[serde(default)]
    author: Option<GhActor>,
    state: String,
    #[serde(default)]
    submitted_at: Option<Timestamp>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    commit: Option<GhOid>,
    comments: Nodes<GhReviewComment>,
}

#[derive(Deserialize)]
struct GhActor {
    login: String,
}

#[derive(Deserialize)]
struct GhOid {
    oid: String,
}

#[derive(Deserialize)]
struct GhReviewComment {
    path: String,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhRateLimit {
    cost: u64,
    remaining: u64,
    reset_at: Timestamp,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BaseData {
    repository: BaseRepository,
    #[serde(default)]
    rate_limit: Option<GhRateLimit>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BaseRepository {
    head: Option<CommitObject>,
    #[serde(default)]
    at_push: Option<CommitObject>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitData {
    repository: CommitRepository,
    #[serde(default)]
    rate_limit: Option<GhRateLimit>,
}

#[derive(Deserialize)]
struct CommitRepository {
    object: Option<CommitObject>,
}

/// An object by expression: a commit carries its `oid`; any other object
/// answers an empty fragment.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitObject {
    #[serde(default)]
    oid: Option<String>,
    #[serde(default)]
    status_check_rollup: Option<Rollup>,
}

impl CommitObject {
    fn into_commit(self) -> Option<(CommitId, Option<Rollup>)> {
        let oid = CommitId::new(self.oid?).ok()?;
        Some((oid, self.status_check_rollup))
    }
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
    #[serde(default)]
    mergeable: Option<String>,
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
        #[serde(default)]
        database_id: Option<u64>,
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
        description: Option<String>,
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
                database_id,
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
                    id: database_id,
                    description: None,
                }
            }
            Self::StatusContext {
                context,
                state,
                description,
                is_required,
                target_url,
            } => Check {
                kind: CheckKind::Status,
                name: context,
                state: status_state(&state),
                raw: state,
                required: is_required.unwrap_or(false),
                url: target_url,
                id: None,
                description,
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
            ..Default::default()
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
        // A 403 without the rate-limit headers is a refusal, not a limit.
        let refused = error(403, r#"{"message":"Resource not accessible"}"#);
        assert_eq!(refused.kind, ErrorKind::Other);
        assert!(refused.message.contains("403 Resource"), "{refused}");
        assert_eq!(error(502, "<html>").kind, ErrorKind::Other);
        assert_eq!(error(200, r#"{"data":null}"#).kind, ErrorKind::Other);
    }

    /// OWL-209: a spent budget is its own kind, with its reset when the
    /// answer gives one.
    #[test]
    fn a_spent_budget_is_rate_limited_until_its_reset() {
        let limited = |status, rate: RateHeaders, body: &str| {
            decode::<Value>(&Response {
                status,
                body: body.to_owned(),
                rate,
            })
            .unwrap_err()
        };
        let spent = RateHeaders {
            remaining: Some(0),
            reset: Some(1_790_000_000),
            retry_after: None,
        };
        let e = limited(403, spent, r#"{"message":"API rate limit exceeded"}"#);
        assert_eq!(e.kind, ErrorKind::RateLimited);
        assert_eq!(
            e.resets_at,
            Some(Timestamp::from_second(1_790_000_000).unwrap())
        );
        assert!(e.message.contains("API rate limit exceeded"), "{e}");
        // A secondary limit: Retry-After, any body.
        let retry = RateHeaders {
            retry_after: Some(60),
            ..RateHeaders::default()
        };
        let e = limited(429, retry, "<html>");
        assert_eq!(e.kind, ErrorKind::RateLimited);
        assert!(e.resets_at.is_some_and(|at| at > Timestamp::now()));
        // GraphQL's own: HTTP 200 with an error; no reset in the answer.
        let graphql = r#"{"data":null,"errors":[{"type":"RATE_LIMITED","message":"API rate limit exceeded for user"}]}"#;
        let e = limited(200, RateHeaders::default(), graphql);
        assert_eq!(e.kind, ErrorKind::RateLimited);
        assert_eq!(e.resets_at, None);
        let now = Timestamp::from_second(1_000).unwrap();
        assert_eq!(
            resets_at(retry, now),
            Some(Timestamp::from_second(1_060).unwrap())
        );
        assert_eq!(resets_at(RateHeaders::default(), now), None);
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
        // With the head read (OWL-209).
        assert_eq!(e.head, Some(CommitId::new(OTHER).unwrap()));
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

    /// Watch's read takes the head it finds, keeps it across pages, and
    /// lists no check of a pull request that is not open.
    #[test]
    fn watchs_read_takes_the_head_it_finds() {
        let (forge, canned) = forge(vec![
            page(OTHER, OTHER, 2, RUN, Some("c1")),
            page(OTHER, OTHER, 2, STATUS, None),
        ]);
        let read = forge.with_check_page(1).follow(7).unwrap();
        assert!(
            canned.requests.lock().unwrap()[0]["query"]
                .as_str()
                .unwrap()
                .starts_with("query Follow")
        );
        assert_eq!(read.checks.head, CommitId::new(OTHER).unwrap());
        assert_eq!(read.checks.checks.len(), 2);
        assert_eq!(read.rate, None);
        let moved = forge_of(vec![
            page(HEAD, HEAD, 2, RUN, Some("c1")),
            page(OTHER, OTHER, 2, RUN, None),
        ])
        .with_check_page(1)
        .follow(7)
        .unwrap_err();
        assert_eq!(moved.kind, ErrorKind::HeadMoved);
        let merged = page(HEAD, OTHER, 9, RUN, Some("c1"))
            .replace(r#""state":"OPEN""#, r#""state":"MERGED""#)
            .replace(r#""mergeable":"MERGEABLE""#, r#""mergeable":null"#);
        let read = forge_of(vec![merged]).follow(7).unwrap();
        assert_eq!(read.checks.state, PrState::Merged);
        assert!(read.checks.checks.is_empty());
        assert_eq!(read.checks.mergeable, Mergeable::Unknown);
    }

    fn forge_of(bodies: Vec<String>) -> GitHubForge {
        forge(bodies).0
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

    /// OWL-334: a pull request GitHub cannot open on unreadable refs says
    /// what the token probably lacks; another refusal is left as it is.
    #[test]
    fn unreadable_refs_name_the_missing_permission() {
        let head = Branch::new("owl-17").unwrap();
        let base = Branch::new("main").unwrap();
        let new = NewPullRequest {
            head: &head,
            base: &base,
            title: "t",
            body: "b",
            draft: false,
        };
        let refused = |message: &str| {
            let (forge, _) = forge(vec![
                r#"{"data":{"repository":{"id":"R_1"}}}"#.to_owned(),
                format!(
                    r#"{{"data":{{"createPullRequest":null}},"errors":[{{"type":"UNPROCESSABLE","message":"{message}"}}]}}"#
                ),
            ]);
            forge.open_pull_request(new).unwrap_err()
        };
        let unreadable = refused("not all refs are readable");
        assert_eq!(unreadable.kind, ErrorKind::Other);
        assert!(
            unreadable
                .message
                .starts_with("GitHub: UNPROCESSABLE: not all refs are readable (")
                && unreadable.message.contains("lacks contents (read)"),
            "{unreadable}"
        );
        let exists = refused("A pull request already exists");
        assert_eq!(
            exists.message,
            "GitHub: UNPROCESSABLE: A pull request already exists"
        );
    }

    /// Answers every request with one HTTP answer, or with no answer.
    struct Fixed(Result<Response, String>);

    impl Transport for Fixed {
        fn send(&self, _request: &Value) -> Result<Response, String> {
            self.0.clone()
        }
    }

    fn default_branch(answer: Result<Response, String>) -> Result<DefaultBranch, Error> {
        let repo = Repo::parse("pitchopp/locary").unwrap();
        GitHubForge::with_transport(Fixed(answer), repo).default_branch()
    }

    /// OWL-334: the default branch read tells a readable ref, a repository
    /// with none, a token refused its contents, and the errors of any other
    /// read apart.
    #[test]
    fn the_default_branch_read_tells_a_refused_token_apart() {
        let ok = |body: &str| default_branch(Ok(answer(200, body)));
        let readable = format!(
            r#"{{"data":{{"repository":{{"defaultBranchRef":{{"name":"develop","target":{{"oid":"{HEAD}"}}}}}}}}}}"#
        );
        assert_eq!(
            ok(&readable),
            Ok(DefaultBranch::Readable(Branch::new("develop").unwrap()))
        );
        assert_eq!(
            ok(r#"{"data":{"repository":{"defaultBranchRef":null}}}"#),
            Ok(DefaultBranch::Empty)
        );
        // The shape GitHub answered on 2026-10-17 to a token without
        // contents (read).
        let forbidden = r#"{"data":{"repository":{"defaultBranchRef":null}},"errors":[{"type":"FORBIDDEN","path":["repository","defaultBranchRef"],"message":"Resource not accessible by personal access token"}]}"#;
        assert_eq!(
            ok(forbidden),
            Ok(DefaultBranch::Forbidden(
                "GitHub: FORBIDDEN: Resource not accessible by personal access token".into()
            ))
        );
        let rest_refusal = default_branch(Ok(answer(
            403,
            r#"{"message":"Resource not accessible by personal access token"}"#,
        )));
        assert!(
            matches!(&rest_refusal, Ok(DefaultBranch::Forbidden(m)) if m.contains("403 Resource")),
            "{rest_refusal:?}"
        );

        let kind = |result: Result<DefaultBranch, Error>| result.unwrap_err().kind;
        assert_eq!(
            kind(default_branch(Ok(answer(
                401,
                r#"{"message":"Bad credentials"}"#
            )))),
            ErrorKind::Unauthorized
        );
        let missing = r#"{"data":{"repository":null},"errors":[{"type":"NOT_FOUND","message":"Could not resolve"}]}"#;
        assert_eq!(kind(ok(missing)), ErrorKind::NotFound);
        assert_eq!(
            kind(ok(r#"{"data":{"repository":null}}"#)),
            ErrorKind::NotFound
        );
        let unreachable = default_branch(Err("connection refused".into())).unwrap_err();
        assert_eq!(unreachable.kind, ErrorKind::Other);
        assert_eq!(unreachable.message, "GitHub: connection refused");
        // A spent budget stays one, whatever else the answer says.
        let spent = Response {
            status: 403,
            body: r#"{"message":"API rate limit exceeded"}"#.into(),
            rate: RateHeaders {
                remaining: Some(0),
                ..Default::default()
            },
        };
        assert_eq!(kind(default_branch(Ok(spent))), ErrorKind::RateLimited);
        // A secondary limit may answer 403 with neither header: its words
        // keep it from passing for a refused permission.
        let secondary = default_branch(Ok(answer(
            403,
            r#"{"message":"You have exceeded a secondary rate limit. Please wait."}"#,
        )));
        assert!(
            matches!(&secondary, Err(e) if e.message.contains("secondary rate limit")),
            "{secondary:?}"
        );
        let both = r#"{"data":null,"errors":[{"type":"RATE_LIMITED","message":"slow down"},{"type":"FORBIDDEN","message":"no"}]}"#;
        assert_eq!(kind(ok(both)), ErrorKind::RateLimited);
        // A ref without its commit is not read whole.
        let no_oid =
            r#"{"data":{"repository":{"defaultBranchRef":{"name":"main","target":{"oid":"zz"}}}}}"#;
        assert_eq!(kind(ok(no_oid)), ErrorKind::Other);
    }

    #[test]
    fn the_token_never_shows_in_debug_output() {
        let token = Token::new("ghp_do_not_print");
        let transport = HttpTransport::new(token.clone());
        let forge = GitHubForge::new(token.clone(), Repo::parse("o/n").unwrap());
        let shown = format!("{token:?} {transport:?} {forge:?}");
        assert!(!shown.contains("do_not_print"), "{shown}");
    }

    /// A log as GitHub Actions writes it: timestamps, an ANSI escape, the
    /// error line, and cleanup after it.
    const LOG: &str = "2026-10-01T09:12:40.1000000Z ##[group]Run cargo clippy\n\
        2026-10-01T09:12:44.1234567Z \u{1b}[1m\u{1b}[91merror\u{1b}[0m: unused import: `std::fs`\n\
        2026-10-01T09:12:44.2000000Z ##[error]Process completed with exit code 101.\n\
        2026-10-01T09:12:45.0000000Z Post job cleanup.\n";

    /// The window ends with the first `##[error]` line, cleaned; without
    /// one, it is the log's end; a window keeps whole lines within its
    /// bytes, and a single longer line keeps its end.
    #[test]
    fn a_logs_excerpt_ends_with_its_first_error() {
        let excerpt = log_excerpt(LOG.as_bytes(), 1024).unwrap();
        assert_eq!(
            excerpt,
            "##[group]Run cargo clippy\nerror: unused import: `std::fs`\n\
             ##[error]Process completed with exit code 101.\n"
        );
        let excerpt = log_excerpt(LOG.as_bytes(), 60).unwrap();
        assert_eq!(excerpt, "##[error]Process completed with exit code 101.\n");

        let quiet = "a\nb\nc\n".repeat(1000);
        assert_eq!(log_excerpt(quiet.as_bytes(), 5).unwrap(), "b\nc\n");
        let long = format!("{}é\n", "x".repeat(100));
        assert_eq!(log_excerpt(long.as_bytes(), 4).unwrap(), "xé\n");
        // A log past the bytes read keeps the end of what was read.
        let big = format!("{}tail\n", "line\n".repeat(100_000));
        let read = big.as_bytes().take(big.len() as u64 - 5);
        assert_eq!(log_excerpt(read, 10).unwrap(), "line\nline\n");
    }

    /// Only an HTTPS redirect without a credential of its own is followed;
    /// a signed URL's query never shows in an error.
    #[test]
    fn a_redirect_is_followed_to_https_only_and_its_query_is_redacted() {
        let signed = "https://results.blob.core.windows.net/logs/job.txt?sig=SECRET&se=1";
        assert_eq!(redirect_target(200, None), Ok(None));
        assert_eq!(
            redirect_target(302, Some(signed)),
            Ok(Some(signed.to_owned()))
        );
        assert!(redirect_target(302, None).is_err());
        let plain = redirect_target(302, Some("http://example.com/x?sig=SECRET")).unwrap_err();
        assert!(!plain.contains("SECRET"), "{plain}");
        assert!(redirect_target(302, Some("https://user:pw@example.com/x")).is_err());
        assert!(redirect_target(302, Some("https:///x")).is_err());
        let error = redact(&format!("failed to GET {signed} timeout? yes"));
        assert_eq!(
            error,
            "failed to GET https://results.blob.core.windows.net/logs/job.txt?<query removed> \
             timeout? yes"
        );
    }

    /// Answers REST reads with one status and body each.
    struct Rest(Mutex<Vec<(u16, RateHeaders, &'static str)>>);

    impl Transport for Rest {
        fn send(&self, _: &Value) -> Result<Response, String> {
            Err("no GraphQL here".to_owned())
        }

        fn get(&self, path: &str) -> Result<RestAnswer, String> {
            assert!(path.starts_with("repos/o/n/"), "{path}");
            let (status, rate, body) = self.0.lock().unwrap().remove(0);
            Ok(RestAnswer {
                rate,
                ..RestAnswer::new(status, body)
            })
        }
    }

    /// A job's log and another app's output are read by REST; a refused
    /// token, a spent budget and a gone log are told apart.
    #[test]
    fn rest_reads_give_the_excerpt_or_why_not() {
        let spent = RateHeaders {
            remaining: Some(0),
            reset: Some(1_790_000_000),
            retry_after: None,
        };
        let none = RateHeaders::default();
        let forge = GitHubForge::with_transport(
            Rest(Mutex::new(vec![
                (200, none, LOG),
                (
                    403,
                    none,
                    r#"{"message":"Resource not accessible by personal access token"}"#,
                ),
                (403, spent, r#"{"message":"API rate limit exceeded"}"#),
                (410, none, ""),
                (
                    200,
                    none,
                    r#"{"output":{"title":"Coverage","summary":"71%","text":null}}"#,
                ),
            ])),
            Repo::parse("o/n").unwrap(),
        );
        assert!(
            forge
                .job_log_excerpt(2)
                .unwrap()
                .ends_with("exit code 101.\n")
        );
        let refused = forge.job_log_excerpt(2).unwrap_err();
        assert_eq!(refused.kind, ErrorKind::Unauthorized);
        assert!(
            refused.message.contains("personal access token"),
            "{refused}"
        );
        let limited = forge.job_log_excerpt(2).unwrap_err();
        assert_eq!(limited.kind, ErrorKind::RateLimited);
        assert_eq!(limited.resets_at.unwrap().as_second(), 1_790_000_000);
        assert_eq!(
            forge.job_log_excerpt(2).unwrap_err().kind,
            ErrorKind::NotFound
        );
        assert_eq!(forge.check_run_output(3).unwrap(), "Coverage\n\n71%");
    }
}
