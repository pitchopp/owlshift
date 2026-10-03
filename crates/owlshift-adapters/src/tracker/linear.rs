//! The Linear tracker: Linear's GraphQL API over HTTPS, authenticated with a
//! personal API key that the runner reads from the system keychain.
//!
//! The adapter talks through a [`Transport`], one GraphQL request at a time:
//! [`HttpTransport`] in production, a replay of recorded exchanges in tests
//! (see [`crate::graphql`]).
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
//! Checked live on 2026-09-29 (OWL-62), by schema introspection and a read of
//! the workspace's issues:
//!
//! - `Issue.creator` is the account that created the issue, null when that
//!   account was deleted or an integration or system process created it; the
//!   issue's `botActor` or `externalUserCreator` then names it. OWL-1 to OWL-4,
//!   made by Linear's onboarding, have a null creator and the bot actor
//!   `Linear` (type `workflow`); every other issue has a creator, and those
//!   read (the 50 most recent, OWL-11, OWL-13) have the account of the
//!   personal API key and no bot actor.
//!
//! Checked live on 2026-09-29 (OWL-74; build plan, check C4): an issue typed
//! by hand in Linear's app (OWL-82) and issues an agent created through the
//! same account's personal API key (OWL-80, OWL-81, OWL-74) answer alike on
//! every field compared, and the schema has no field naming the client. So a
//! ticket's creator is never reported as an account (`ticket_author`).
//!
//! Checked live on 2026-10-04 (OWL-137; build plan, check C4): the issue's
//! team lists its workflow states (nine for `OWL`, one page), and
//! `issueUpdate` with a state's id moves the issue and answers it in that
//! state, through the same personal API key.
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

pub use crate::graphql::{Response, Transport};
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
    pub const CAPABILITIES: &'static [Capability] = &[
        Capability::ReadTicket,
        Capability::Comments,
        Capability::VisibleStage,
    ];

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
            author: ticket_author(issue.creator, issue.bot_actor, issue.external_user_creator),
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

    /// Two requests: the workflow states of the issue's team, to find the
    /// one named `state`, then `issueUpdate` with its id. The issue is
    /// updated by the id Linear gave in the first answer. Nothing is cached:
    /// a command moves the stage a few times at most.
    fn set_stage(&self, id: &TicketId, state: &str) -> Result<(), Error> {
        let data: StatesData = self.call(STATES_QUERY, json!({ "id": id.as_str() }))?;
        let issue = data.issue;
        let team = issue.team;
        if team.states.page_info.has_next_page {
            return Err(invalid(format!(
                "team {} has more than {MAX_PAGE} workflow states, which this adapter does not \
                 read",
                team.key
            )));
        }
        let named: Vec<&WorkflowState> = team
            .states
            .nodes
            .iter()
            .filter(|s| s.name == state)
            .collect();
        let target = match named[..] {
            [one] => one,
            [] => {
                let names: Vec<&str> = team.states.nodes.iter().map(|s| s.name.as_str()).collect();
                return Err(invalid(format!(
                    "team {} has no workflow state named {state:?}: the project's \
                     `[tracker].states` must name one of {}",
                    team.key,
                    names.join(", ")
                )));
            }
            _ => {
                return Err(invalid(format!(
                    "team {} has several workflow states named {state:?}",
                    team.key
                )));
            }
        };
        let variables = json!({ "id": issue.id, "stateId": target.id });
        let data: StageData = self.call(STAGE_MUTATION, variables)?;
        match data.issue_update {
            IssueUpdate {
                success: true,
                issue: Some(IssueState { state: Some(now) }),
            } if now.id == target.id => Ok(()),
            _ => Err(invalid(format!("Linear did not move {id} to {state:?}"))),
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
    creator { id displayName } botActor { name } externalUserCreator { name } \
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

const STATES_QUERY: &str = "query States($id: String!) { issue(id: $id) { id \
    team { key states(first: 50) { nodes { id name } pageInfo { hasNextPage } } } } }";

const STAGE_MUTATION: &str = "mutation SetStage($id: String!, $stateId: String!) { \
    issueUpdate(id: $id, input: { stateId: $stateId }) { success issue { state { id name } } } }";

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
    creator: Option<User>,
    bot_actor: Option<Named>,
    external_user_creator: Option<Named>,
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

/// Who wrote a comment, and the base of [`ticket_author`]. It is an account
/// only when Linear names an account and nothing else: a bot or an external
/// user alongside it means an integration acted, and what it carried is not
/// the account's own text, so the author is then that bot or external user.
/// The recorded answers never set both; the rule fails closed if Linear ever
/// does. Nobody named (a deleted account) is [`Author::unknown`].
///
/// A comment posted through an account's personal API key is that account's
/// too: a limit accepted for comments, which are how the decider answers.
fn author(user: Option<User>, bot: Option<Named>, external: Option<Named>) -> Author {
    match (user, bot, external) {
        (Some(user), None, None) => Author::Account(user.into_person()),
        (user, bot, external) => bot
            .and_then(|b| b.name)
            .or_else(|| external.and_then(|e| e.name))
            .or_else(|| user.map(|u| u.display_name))
            .map_or_else(Author::unknown, |name| Author::Other { name }),
    }
}

/// Who created an issue: never an account. Whoever holds an account's
/// personal API key, a script or an agent, creates issues as that account,
/// and Linear answers alike for an issue the account typed by hand (checked
/// 2026-09-29, OWL-74). So the creator keeps its name but reads as an author
/// the adapter cannot vouch for, and a ticket's description is never the
/// decider's instructions. The creator's identifier is still read: the rule
/// for bots and external users, and the recorded query, are shared with
/// comments.
fn ticket_author(creator: Option<User>, bot: Option<Named>, external: Option<Named>) -> Author {
    match author(creator, bot, external) {
        Author::Account(person) => Author::Other { name: person.name },
        other => other,
    }
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
        Comment {
            id: self.id,
            author: author(self.user, self.bot_actor, self.external_user),
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

#[derive(Deserialize)]
struct StatesData {
    issue: IssueTeam,
}

#[derive(Deserialize)]
struct IssueTeam {
    id: String,
    team: Team,
}

#[derive(Deserialize)]
struct Team {
    key: String,
    states: Connection<WorkflowState>,
}

#[derive(Deserialize)]
struct WorkflowState {
    id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StageData {
    issue_update: IssueUpdate,
}

#[derive(Deserialize)]
struct IssueUpdate {
    success: bool,
    issue: Option<IssueState>,
}

#[derive(Deserialize)]
struct IssueState {
    state: Option<WorkflowState>,
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
            "priority":0,"assignee":null,"creator":null,"botActor":null,
            "externalUserCreator":null,"labels":{"nodes":[],"pageInfo":{"hasNextPage":true}}}}}"#;
        assert!(tracker(vec![many_labels]).ticket(&id).is_err());

        let stuck = r#"{"data":{"issue":{"comments":{"nodes":[],
            "pageInfo":{"hasNextPage":true,"endCursor":"c1"}}}}}"#;
        assert!(tracker(vec![stuck, stuck]).comments(&id).is_err());

        let refused = r#"{"data":{"commentCreate":{"success":false,"comment":null}}}"#;
        assert!(tracker(vec![refused]).post_comment(&id, "x").is_err());
    }

    /// A stage name is matched exactly among the team's states, and the
    /// update must answer with the state asked for: anything else is an
    /// error, and nothing is written without exactly one match.
    #[test]
    fn a_stage_is_moved_only_to_the_one_state_of_that_name() {
        let id = TicketId::new("OWL-1").unwrap();
        let states = |more: bool, names: &[&str]| -> &'static str {
            let nodes: Vec<Value> = names
                .iter()
                .enumerate()
                .map(|(n, name)| json!({ "id": format!("s{n}"), "name": name }))
                .collect();
            let answer = json!({ "data": { "issue": { "id": "i1", "team": { "key": "OWL",
                "states": { "nodes": nodes, "pageInfo": { "hasNextPage": more } } } } } });
            answer.to_string().leak()
        };
        let moved = |success: bool, state: &str| -> &'static str {
            let answer = json!({ "data": { "issueUpdate": { "success": success,
                "issue": { "state": { "id": state, "name": "x" } } } } });
            answer.to_string().leak()
        };
        let both = states(false, &["Todo", "Needs Input"]);

        assert!(
            tracker(vec![both, moved(true, "s1")])
                .set_stage(&id, "Needs Input")
                .is_ok()
        );
        // Exact names only: no case folding.
        let unknown = tracker(vec![both])
            .set_stage(&id, "needs input")
            .unwrap_err();
        assert!(
            unknown
                .message
                .contains("must name one of Todo, Needs Input"),
            "{unknown}"
        );
        let twice = states(false, &["Doing", "Doing"]);
        assert!(tracker(vec![twice]).set_stage(&id, "Doing").is_err());
        let truncated = states(true, &["Doing"]);
        assert!(tracker(vec![truncated]).set_stage(&id, "Doing").is_err());
        assert!(
            tracker(vec![both, moved(false, "s1")])
                .set_stage(&id, "Needs Input")
                .is_err()
        );
        assert!(
            tracker(vec![both, moved(true, "s0")])
                .set_stage(&id, "Needs Input")
                .is_err()
        );
    }

    /// OWL-1 as Linear gave it on 2026-09-29, made by its onboarding
    /// workflow: no creator, a bot actor.
    #[test]
    fn a_ticket_made_by_a_bot_is_attributed_to_the_bot() {
        let owl_1 = r#"{"data":{"issue":{"identifier":"OWL-1","title":"t","description":null,
            "priority":0,"assignee":null,"creator":null,"botActor":{"name":"Linear"},
            "externalUserCreator":null,"labels":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}"#;
        let ticket = tracker(vec![owl_1])
            .ticket(&TicketId::new("OWL-1").unwrap())
            .unwrap();
        assert_eq!(
            ticket.author,
            Author::Other {
                name: "Linear".to_owned()
            }
        );
    }

    /// An account is the author only when Linear names nothing else beside
    /// it: an integration acting for an account never reads as the account.
    #[test]
    fn an_account_beside_a_bot_or_an_external_user_is_not_the_author() {
        let user = || {
            Some(User {
                id: "u1".to_owned(),
                display_name: "person-1".to_owned(),
            })
        };
        let named = |name: Option<&str>| {
            Some(Named {
                name: name.map(str::to_owned),
            })
        };
        let other = |name: &str| Author::Other {
            name: name.to_owned(),
        };
        assert_eq!(author(user(), named(Some("Slack")), None), other("Slack"));
        assert_eq!(
            author(user(), None, named(Some("Customer"))),
            other("Customer")
        );
        assert_eq!(author(user(), named(None), None), other("person-1"));
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
