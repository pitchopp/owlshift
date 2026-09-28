//! The Linear tracker: Linear's GraphQL API over HTTPS, authenticated with a
//! personal API key that the runner reads from the system keychain.
//!
//! The adapter talks through a [`Transport`], one GraphQL request at a time:
//! [`HttpTransport`] in production, a replay of recorded exchanges in tests.
//! An exchange is the request body, the HTTP status and the response body:
//! never a header, so a recording cannot hold the key.
//!
//! What the adapter relies on was checked live on the Owlshift workspace on
//! 2026-09-28 (OWL-13; build plan, check C4):
//!
//! - `Issue.priority` is 0 (none), 1 (urgent), 2 (high), 3 (medium) or 4 (low).
//! - `Comment.editedAt` is the last edit by the comment's author, null when
//!   never edited. `Comment.updatedAt` is not an edit time: on every comment
//!   observed it differs from `createdAt` although nobody edited it.
//! - `issue.comments` lists every comment, replies and inline comments on the
//!   description included, newest first whatever `orderBy` says, so the
//!   adapter sorts them oldest first itself.
//! - An unknown issue answers HTTP 200 with a GraphQL error of code
//!   `INPUT_ERROR` and a message saying "not found" ("Entity not found: Issue"
//!   on a query, "issue not found" on `commentCreate`). A rejected key
//!   answers HTTP 401 with code `AUTHENTICATION_ERROR`.
//! - `commentCreate` accepts an issue identifier such as `OWL-13`.
//!
//! A rate-limited answer was not observed; it surfaces as
//! [`ErrorKind::Other`] with Linear's code and message.

use std::fmt;
use std::time::Duration;

use jiff::Timestamp;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use owlshift_contracts::Priority;
use owlshift_contracts::ids::TicketId;

use crate::tracker::{Author, Capability, Comment, Error, ErrorKind, Person, Ticket, Tracker};

/// Linear's GraphQL endpoint.
pub const ENDPOINT: &str = "https://api.linear.app/graphql";

/// The most items Linear returns in one page.
pub const MAX_PAGE: u32 = 50;

/// A Linear personal API key. It never shows in `Debug` output.
#[derive(Clone)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

/// An HTTP answer: its status and its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// Sends one GraphQL request to Linear.
pub trait Transport {
    /// Posts `request`, a GraphQL request body (`query` and `variables`).
    /// `Err` means no HTTP answer at all: network, TLS or timeout.
    fn send(&self, request: &Value) -> Result<Response, String>;
}

/// The production transport: HTTPS to [`ENDPOINT`] with the API key.
pub struct HttpTransport {
    agent: ureq::Agent,
    key: ApiKey,
}

impl fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTransport")
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

impl HttpTransport {
    pub fn new(key: ApiKey) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            // A 401 carries a GraphQL error body worth reading.
            .http_status_as_error(false)
            .user_agent(concat!("owlshift/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            key,
        }
    }
}

impl Transport for HttpTransport {
    fn send(&self, request: &Value) -> Result<Response, String> {
        let mut response = self
            .agent
            .post(ENDPOINT)
            .header("Authorization", &self.key.0)
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

/// A Linear workspace, as seen through one API key.
pub struct LinearTracker {
    transport: Box<dyn Transport + Send + Sync>,
    comment_page: u32,
}

impl fmt::Debug for LinearTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinearTracker")
            .field("comment_page", &self.comment_page)
            .finish_non_exhaustive()
    }
}

impl LinearTracker {
    /// What the Linear adapter implements in this build.
    pub const CAPABILITIES: &'static [Capability] = &[Capability::ReadTicket, Capability::Comments];

    /// The adapter over HTTPS with this key.
    pub fn new(key: ApiKey) -> Self {
        Self::with_transport(HttpTransport::new(key))
    }

    /// The adapter over another transport, such as a test replay.
    pub fn with_transport(transport: impl Transport + Send + Sync + 'static) -> Self {
        Self {
            transport: Box::new(transport),
            comment_page: MAX_PAGE,
        }
    }

    /// Reads comments `size` at a time (1 to [`MAX_PAGE`], the default), so a
    /// recording can cover pagination on a ticket with few comments.
    pub fn with_comment_page(mut self, size: u32) -> Self {
        self.comment_page = size.clamp(1, MAX_PAGE);
        self
    }

    /// Sends one request and returns its `data`, decoded.
    fn call<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T, Error> {
        let request = json!({ "query": query, "variables": variables });
        let response = self
            .transport
            .send(&request)
            .map_err(|e| Error::new(ErrorKind::Other, format!("Linear: {e}")))?;
        decode(&response)
    }
}

impl Tracker for LinearTracker {
    fn capabilities(&self) -> &'static [Capability] {
        Self::CAPABILITIES
    }

    fn ticket(&self, id: &TicketId) -> Result<Ticket, Error> {
        let data: TicketData = self.call(TICKET_QUERY, json!({ "id": id.as_str() }))?;
        let issue = data.issue;
        if issue.labels.page_info.has_next_page {
            return Err(invalid(format!(
                "{id} has more than {MAX_PAGE} labels, which this adapter does not read"
            )));
        }
        Ok(Ticket {
            id: TicketId::new(issue.identifier).map_err(|e| invalid(e.to_string()))?,
            title: issue.title,
            description: issue.description.unwrap_or_default(),
            priority: priority(issue.priority)?,
            assignee: issue.assignee.map(User::into_person),
            labels: issue.labels.nodes.into_iter().map(|l| l.name).collect(),
        })
    }

    fn comments(&self, id: &TicketId) -> Result<Vec<Comment>, Error> {
        let mut comments = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let variables =
                json!({ "id": id.as_str(), "first": self.comment_page, "after": after });
            let data: CommentsData = self.call(COMMENTS_QUERY, variables)?;
            let page = data.issue.comments;
            comments.extend(page.nodes.into_iter().map(LinearComment::into_comment));
            if !page.page_info.has_next_page {
                break;
            }
            match page.page_info.end_cursor {
                Some(cursor) if after.as_ref() != Some(&cursor) => after = Some(cursor),
                _ => return Err(invalid("the comment pages do not advance")),
            }
        }
        // Linear lists newest first; the contract is oldest first.
        comments.sort_by_key(|c| c.created_at);
        Ok(comments)
    }

    fn post_comment(&self, id: &TicketId, body: &str) -> Result<Comment, Error> {
        let variables = json!({ "issueId": id.as_str(), "body": body });
        let data: PostData = self.call(POST_MUTATION, variables)?;
        match data.comment_create {
            CommentCreate {
                success: true,
                comment: Some(comment),
            } => Ok(comment.into_comment()),
            _ => Err(invalid(format!(
                "Linear did not create the comment on {id}"
            ))),
        }
    }
}

/// The fields read from every comment. No e-mail, no full name: only what the
/// contract carries.
macro_rules! comment_fields {
    () => {
        "id body createdAt editedAt user { id displayName } botActor { name } externalUser { name }"
    };
}

const TICKET_QUERY: &str = "query Ticket($id: String!) { issue(id: $id) { \
    identifier title description priority assignee { id displayName } \
    labels(first: 50) { nodes { name } pageInfo { hasNextPage } } } }";

const COMMENTS_QUERY: &str = concat!(
    "query Comments($id: String!, $first: Int!, $after: String) { issue(id: $id) { \
     comments(first: $first, after: $after) { nodes { ",
    comment_fields!(),
    " } pageInfo { hasNextPage endCursor } } } }"
);

const POST_MUTATION: &str = concat!(
    "mutation PostComment($issueId: String!, $body: String!) { \
     commentCreate(input: { issueId: $issueId, body: $body }) { success comment { ",
    comment_fields!(),
    " } } }"
);

/// Reads a GraphQL answer: its errors first, then its `data`.
fn decode<T: DeserializeOwned>(response: &Response) -> Result<T, Error> {
    let Ok(envelope) = serde_json::from_str::<Envelope>(&response.body) else {
        let kind = if response.status == 401 {
            ErrorKind::Unauthorized
        } else {
            ErrorKind::Other
        };
        return Err(Error::new(
            kind,
            format!(
                "Linear answered HTTP {} with a body that is not GraphQL",
                response.status
            ),
        ));
    };
    if let Some(error) = envelope.errors.into_iter().next() {
        return Err(classify(response.status, error));
    }
    if response.status != 200 {
        return Err(invalid(format!("Linear answered HTTP {}", response.status)));
    }
    match envelope.data {
        Some(data) if !data.is_null() => serde_json::from_value(data)
            .map_err(|e| invalid(format!("unexpected answer from Linear: {e}"))),
        _ => Err(invalid("Linear answered without data")),
    }
}

/// Classifies a GraphQL error by its code, and for `INPUT_ERROR` by the
/// "not found" of its message, as recorded on 2026-09-28.
fn classify(status: u16, error: GraphQlError) -> Error {
    let code = error.extensions.and_then(|e| e.code).unwrap_or_default();
    let kind = if status == 401 || code == "AUTHENTICATION_ERROR" {
        ErrorKind::Unauthorized
    } else if code == "INPUT_ERROR" && error.message.to_lowercase().contains("not found") {
        ErrorKind::NotFound
    } else {
        ErrorKind::Other
    };
    let code = if code.is_empty() { "no code" } else { &code };
    Error::new(kind, format!("Linear: {code}: {}", error.message))
}

fn priority(value: f64) -> Result<Priority, Error> {
    Ok(match value {
        0.0 => Priority::Unset,
        1.0 => Priority::Urgent,
        2.0 => Priority::High,
        3.0 => Priority::Medium,
        4.0 => Priority::Low,
        other => return Err(invalid(format!("unknown Linear priority {other}"))),
    })
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
}

#[derive(Deserialize)]
struct GraphQlError {
    message: String,
    #[serde(default)]
    extensions: Option<Extensions>,
}

#[derive(Deserialize)]
struct Extensions {
    #[serde(default)]
    code: Option<String>,
}

#[derive(Deserialize)]
struct TicketData {
    issue: Issue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Issue {
    identifier: String,
    title: String,
    description: Option<String>,
    priority: f64,
    assignee: Option<User>,
    labels: Connection<Label>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Connection<T> {
    nodes: Vec<T>,
    page_info: PageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    #[serde(default)]
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct User {
    id: String,
    display_name: String,
}

impl User {
    fn into_person(self) -> Person {
        Person {
            id: self.id,
            name: self.display_name,
        }
    }
}

#[derive(Deserialize)]
struct Named {
    name: Option<String>,
}

#[derive(Deserialize)]
struct CommentsData {
    issue: IssueComments,
}

#[derive(Deserialize)]
struct IssueComments {
    comments: Connection<LinearComment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinearComment {
    id: String,
    body: String,
    created_at: Timestamp,
    edited_at: Option<Timestamp>,
    user: Option<User>,
    bot_actor: Option<Named>,
    external_user: Option<Named>,
}

impl LinearComment {
    fn into_comment(self) -> Comment {
        let author = match (self.user, self.bot_actor, self.external_user) {
            (Some(user), _, _) => Author::Account(user.into_person()),
            (None, Some(Named { name: Some(name) }), _)
            | (None, _, Some(Named { name: Some(name) })) => Author::Other { name },
            _ => Author::Other {
                name: "unknown".to_owned(),
            },
        };
        Comment {
            id: self.id,
            author,
            created_at: self.created_at,
            edited_at: self.edited_at,
            body: self.body,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostData {
    comment_create: CommentCreate,
}

#[derive(Deserialize)]
struct CommentCreate {
    success: bool,
    comment: Option<LinearComment>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(status: u16, body: &str) -> Response {
        Response {
            status,
            body: body.to_owned(),
        }
    }

    fn error_kind(status: u16, body: &str) -> ErrorKind {
        decode::<Value>(&answer(status, body)).unwrap_err().kind
    }

    #[test]
    fn graphql_errors_are_classified_by_code() {
        let other_input = r#"{"errors":[{"message":"Argument Validation Error","extensions":{"code":"INVALID_INPUT"}}],"data":null}"#;
        assert_eq!(error_kind(200, other_input), ErrorKind::Other);
        let ratelimited =
            r#"{"errors":[{"message":"Rate limit exceeded","extensions":{"code":"RATELIMITED"}}]}"#;
        let error = decode::<Value>(&answer(400, ratelimited)).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Other);
        assert_eq!(error.message, "Linear: RATELIMITED: Rate limit exceeded");
        let other_input_error =
            r#"{"errors":[{"message":"Title is too long","extensions":{"code":"INPUT_ERROR"}}]}"#;
        assert_eq!(error_kind(200, other_input_error), ErrorKind::Other);
    }

    #[test]
    fn answers_that_are_not_graphql_are_errors() {
        assert_eq!(error_kind(401, "Unauthorized"), ErrorKind::Unauthorized);
        assert_eq!(
            error_kind(502, "<html>Bad gateway</html>"),
            ErrorKind::Other
        );
        assert_eq!(error_kind(500, r#"{"data":null}"#), ErrorKind::Other);
        assert_eq!(error_kind(200, r#"{"data":null}"#), ErrorKind::Other);
    }

    #[test]
    fn priorities_map_onto_the_vocabulary() {
        let mapped: Vec<_> = [0.0, 1.0, 2.0, 3.0, 4.0]
            .into_iter()
            .map(|p| priority(p).unwrap())
            .collect();
        use Priority::*;
        assert_eq!(mapped, [Unset, Urgent, High, Medium, Low]);
        assert!(priority(5.0).is_err());
    }

    #[test]
    fn a_comment_without_an_account_is_attributed_to_what_linear_names() {
        let comment = |user, bot: Option<&str>, external: Option<&str>| LinearComment {
            id: "c".to_owned(),
            body: String::new(),
            created_at: Timestamp::UNIX_EPOCH,
            edited_at: None,
            user,
            bot_actor: bot.map(|n| Named {
                name: Some(n.to_owned()),
            }),
            external_user: external.map(|n| Named {
                name: Some(n.to_owned()),
            }),
        };
        let other = |name: &str| Author::Other {
            name: name.to_owned(),
        };
        assert_eq!(
            comment(None, Some("GitHub"), None).into_comment().author,
            other("GitHub")
        );
        assert_eq!(
            comment(None, None, Some("Slack user"))
                .into_comment()
                .author,
            other("Slack user")
        );
        assert_eq!(
            comment(None, None, None).into_comment().author,
            other("unknown")
        );
    }

    /// Answers with prepared bodies, in order.
    struct Canned(std::sync::Mutex<Vec<&'static str>>);

    impl Transport for Canned {
        fn send(&self, _: &Value) -> Result<Response, String> {
            Ok(answer(200, self.0.lock().unwrap().remove(0)))
        }
    }

    fn tracker(bodies: Vec<&'static str>) -> LinearTracker {
        LinearTracker::with_transport(Canned(std::sync::Mutex::new(bodies)))
    }

    #[test]
    fn what_the_adapter_cannot_represent_is_refused_not_truncated() {
        let id = TicketId::new("OWL-1").unwrap();
        let many_labels = r#"{"data":{"issue":{"identifier":"OWL-1","title":"t","description":null,
            "priority":0,"assignee":null,"labels":{"nodes":[],"pageInfo":{"hasNextPage":true}}}}}"#;
        assert!(tracker(vec![many_labels]).ticket(&id).is_err());

        let stuck = r#"{"data":{"issue":{"comments":{"nodes":[],
            "pageInfo":{"hasNextPage":true,"endCursor":"c1"}}}}}"#;
        assert!(tracker(vec![stuck, stuck]).comments(&id).is_err());

        let refused = r#"{"data":{"commentCreate":{"success":false,"comment":null}}}"#;
        assert!(tracker(vec![refused]).post_comment(&id, "x").is_err());
    }

    #[test]
    fn the_key_never_shows_in_debug_output() {
        let key = ApiKey::new("lin_api_do_not_print");
        let transport = HttpTransport::new(key.clone());
        let tracker = LinearTracker::new(key.clone());
        let shown = format!("{key:?} {transport:?} {tracker:?}");
        assert!(!shown.contains("do_not_print"), "{shown}");
    }
}
