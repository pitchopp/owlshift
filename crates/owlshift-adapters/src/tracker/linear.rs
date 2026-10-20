//! The Linear tracker: Linear's GraphQL API over HTTPS, authenticated with a
//! personal API key that the runner reads from the system keychain, and,
//! when the workspace has given Owlshift an OAuth app, writing as that app's
//! user ([`app`], decision D8).
//!
//! The adapter talks through a [`Transport`], one GraphQL request at a time:
//! [`HttpTransport`] in production, a replay of recorded exchanges in tests
//! (see [`crate::graphql`]). Writes go through a second one when
//! [`LinearTracker::writing_as_app`] set it: [`app::AppTransport`].
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
//! Checked live on 2026-10-04 (OWL-147; build plan, check C4): `teams(filter:
//! { key: { eq: "OWL" } })` answers the one team of that key with its nine
//! workflow states in one page, and an unknown key (`NOPE`) answers
//! `nodes: []`, HTTP 200, no error.
//!
//! Checked live on 2026-10-04 (OWL-153): that key filter is case-sensitive,
//! `eq: "OWL"` answers the team and `eq: "owl"` answers `nodes: []`, HTTP
//! 200, so the key is sent upper case.
//!
//! Checked live on 2026-10-03 (OWL-140): `{ issue(id: "OWL-140") {
//! identifier url } }` answered the `url`
//! `https://linear.app/owlshift/issue/OWL-140/notify-the-operator-on-the-desktop-when-they-become-the-blocker`:
//! the workspace's URL key, the identifier, then a slug of the title. A
//! notification shows it as the ticket's link.
//!
//! Checked live on 2026-10-04 (OWL-157; build plan, check C4), through an
//! OAuth app's client credentials token: a comment posted with it comes from
//! the app user, `user.app` true, with no bot actor or external user, so the
//! adapter reads `app` and never takes such a comment for an account's; the
//! decider got an `issueCommentMention` from the app user for a comment
//! holding the line `Waiting for <their User.url>`, which Linear stored and
//! answered verbatim; `issueUpdate` moved a stage with it; an `issueCreate`
//! through it made an issue whose creator is the app user, `app` true.
//! Checked again on 2026-10-05 through the runner's own code
//! (`owlshift-runner/tests/linear_app_live.rs`): a QUESTIONS round through
//! the app gave the decider an `issueCommentMention` from the app user, and
//! revoking one app token left another of the same scopes valid.
//!
//! Checked live on 2026-10-13 (OWL-246; build plan, "P4 · The drain"),
//! read-only, for the drain's listing and the admission gesture:
//!
//! - `issues(filter: …)` with `team.key`, `state.name` and `labels.name`
//!   each `eq` lists the column, paged by `after` (the Canceled tickets
//!   labelled `agent`, two a page, then the next two). `labels.name.eq` is
//!   case-sensitive: `Agent` lists nothing. A page of the listing costs about
//!   230 complexity points.
//! - `inverseRelations` leaves out an archived blocker unless asked with
//!   `includeArchived: true`: OWL-116 listed none without it, and OWL-114
//!   and OWL-23, both archived, with it. The team archives a closed issue
//!   after a month, so a canceled blocker would otherwise vanish and release
//!   the ticket; the listing asks for them.
//! - `issue.history` lists newest first. Each entry has `actor` (a user,
//!   with `app`), `botActor`, `addedLabels` (null when none) and `toState`.
//!   OWL-129's entry adding `zone:backend/billing` has an account as its
//!   actor, not an app. OWL-179, created with the label `agent`, and
//!   OWL-249, created in Todo, have no entry for it: the creator made the
//!   gesture. The GitHub integration's and Linear's workflow entries have a
//!   `botActor` and no `actor`. One page of 50 entries costs about 411
//!   points.
//!
//! Checked live on 2026-10-14 (OWL-263), read-only, for a state gesture: the
//! moves the runner made through the app (OWL-256, OWL-257, OWL-258: Todo to
//! In Progress, In Progress to Needs Input, Needs Input to In Review) have an
//! `actor` with `app` true, no `botActor`, and both `fromState` and
//! `toState`. OWL-249, created in Todo, has no move of state. No move to Todo
//! by the app was found among the 50 most recently updated issues, so
//! skipping one is tested on made-up histories only.
//!
//! Checked live on 2026-10-14 at 11:02 UTC (OWL-270), read-only, for whose
//! moves a state gesture skips: through a client credentials token of the
//! app (scopes `read,write`, revoked right after), `viewer` answers `app`
//! true and an `id`, and every move of state with an app actor on OWL-256,
//! OWL-257 and OWL-258 (ten in all) has that `id` as its actor's. So the
//! adapter keeps the id that [`LinearTracker::writing_as_app`] reads and
//! skips only that app user's moves. No other app's move was observed: that
//! its actor has another id rests on Linear giving each app its own user.
//!
//! Checked live on 2026-10-13 (OWL-252; build plan, "P4 · The drain",
//! check (e)) for proposals, through the app for the writes:
//!
//! - `issueCreate(input: { teamId, stateId, projectId, title, description })`
//!   with team `OWL`'s Triage state made an issue in Triage, in the project,
//!   created by the app user. `IssueCreateInput` has `labelIds` and no
//!   relation field, so the relations are writes of their own.
//! - `issueRelationCreate(input: { type, issueId, relatedIssueId })` wrote a
//!   `related` relation, and, by a script of its own once that one was
//!   deleted, a `blocks` relation with the parent as `issueId`, which the
//!   proposal then listed in its `inverseRelations`. The two never stand
//!   together (OWL-292, below).
//! - `issues(filter: …, includeArchived: true)` with the team's key and
//!   `title: { eqIgnoreCase }`, or `description: { contains: "fingerprint
//!   <hex>" }`, found the issue, the latter again once it was canceled.
//! - `issueLabels(filter: { name: { eq } })` is case-sensitive (`agent`
//!   answered team `OWL`'s label, `Agent` nothing), each label naming its
//!   team.
//!
//! Checked live on 2026-10-14 (OWL-249; build plan, "P4 · The drain",
//! check (d)) through the API key, on two throwaway issues then canceled:
//!
//! - `issueRelationCreate(input: { type: blocks, issueId, relatedIssueId })`
//!   takes identifiers (`OWL-271` blocking `OWL-272`), and the blocked
//!   issue then lists the relation in its `inverseRelations`.
//! - The same write again answers `success: true` with the same relation:
//!   it is idempotent.
//! - The reverse write answers a new relation, which replaces the first
//!   (OWL-292, below): Linear refuses neither write, and the runner's walk
//!   refuses the reverse one before it is made.
//! - An unknown `issueId` answers `issue not found`, code `INPUT_ERROR`,
//!   which the adapter reads as [`ErrorKind::NotFound`].
//!
//! Checked live on 2026-10-15 (OWL-292; build plan, "P4 · The drain", part
//! I) through the API key, on two throwaway issues then canceled: Linear
//! keeps one relation between two issues. `related` then `blocks`, `blocks`
//! then `related`, and `blocks` one way then the other: each write answered
//! `success: true` and a relation of its own, and read back 5 s later the
//! pair held the later relation only, the earlier one's id answering
//! `Entity not found: IssueRelation`. The proposals the app filed on the P4
//! exit gate kept their `blocks` relation alone in the same way. So a
//! proposal is given one relation.
//!
//! Checked live on 2026-10-19 (OWL-350; build plan, "P5 · Quality pipeline",
//! part Q) through a client credentials token of the app on OWL-350, the
//! personal key having passed the same calls on 2026-10-18:
//!
//! - `documentCreate(input: { issueId, title, content })` made a document
//!   attached to the issue, listed in `issue.documents`, with a `url` and the
//!   app user as its creator; 72,897 characters were accepted.
//! - `documentUpdate(id, input: { content })` replaced its content in place,
//!   same id; read back, Linear had stripped the final line break. 250,000
//!   characters were accepted and 250,001 refused, `INVALID_INPUT`.
//! - Once deleted (`documentDelete`), the document left the issue's listing,
//!   yet `documentUpdate` on it still answered `success: true` with
//!   `archivedAt` set; an unknown id answered `Entity not found: Document`,
//!   `INPUT_ERROR`. So the update asks for `archivedAt`, and an archived
//!   document is not found.
//!
//! Checked live on 2026-10-19 (OWL-371; build plan, "Reactions as OWL-371
//! reads them") through the personal key, for a comment's reactions:
//! `Reaction` has `emoji`, `createdAt`, `user` and `externalUser`, and no
//! `botActor`; `Comment.reactions` is a non-null list. A `+1` made through
//! the key read back as emoji `+1`, its `user` the key's account by `id`
//! and `app` false, through `comment(id)` and `issue.comments` alike; once
//! deleted, neither listed it. The reaction left the comment's `editedAt`
//! as it was and changed its `updatedAt`, which the adapter does not read.
//! Nesting the reactions in a page of 50 comments took its complexity from
//! 9 points to 12.
//!
//! Read only, for `owlshift doctor` (OWL-155, checked 2026-10-14):
//!
//! - `teams(filter: { key })` answers `triageEnabled` and the states with
//!   their `type`; `issueLabels(filter: { name: { in } })` is exact too
//!   (`agent` and `Agent` asked, `agent` answered) and each label has
//!   `team` (null for a workspace label) and `isGroup`;
//!   `users(filter: { id: { in } }, includeDisabled: true)` answers
//!   `active` and `app`, and leaves an unknown id out, where `user(id:)`
//!   fails with an input error.
//!
//! A rate-limited answer was not observed; it surfaces as
//! [`ErrorKind::Other`] with Linear's code and message.

pub mod app;

use std::fmt;
use std::time::Duration;

use jiff::Timestamp;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use owlshift_contracts::Priority;
use owlshift_contracts::config::Admit;
use owlshift_contracts::ids::TicketId;

pub use crate::graphql::{Response, Transport};
use crate::tracker::{
    Author, Blocker, Capability, Column, Comment, Document, Error, ErrorKind, Filed, Found,
    Gesture, Listed, MAX_DOCUMENT_CHARS, PROPOSED_LABEL, Person, Proposal, Reaction, Relations,
    StateType, Ticket, Tracker, no_delegation,
};

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
        Self {
            agent: http_agent(),
            key,
        }
    }
}

impl Transport for HttpTransport {
    fn send(&self, request: &Value) -> Result<Response, String> {
        post_graphql(&self.agent, &self.key.0, request)
    }
}

/// The HTTP agent of every call to Linear, the app's included: 30 seconds
/// at most, and the body of an error status read rather than dropped.
fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        // A 401 carries a GraphQL error body worth reading.
        .http_status_as_error(false)
        .user_agent(concat!("owlshift/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

/// Posts one GraphQL request to [`ENDPOINT`] with `authorization` as the
/// header's value: an API key as it is, or `Bearer` and an app's token. A
/// transport error names no header.
fn post_graphql(
    agent: &ureq::Agent,
    authorization: &str,
    request: &Value,
) -> Result<Response, String> {
    let mut response = agent
        .post(ENDPOINT)
        .header("Authorization", authorization)
        .send_json(request)
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|e| e.to_string())?;
    Ok(Response::new(status, body))
}

type Shared = Box<dyn Transport + Send + Sync>;

/// A Linear workspace, as seen through one API key, writing through an app
/// user when one is set.
pub struct LinearTracker {
    transport: Shared,
    /// The app's transport, for every write, when one is set.
    writes: Option<Shared>,
    /// The id of the app user writing through `writes`, whose moves a state
    /// gesture is read past ([`state_gesture`]).
    app_user: Option<String>,
    comment_page: u32,
    /// The page of the listing and of an issue's history.
    page: u32,
}

impl fmt::Debug for LinearTracker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinearTracker")
            .field("comment_page", &self.comment_page)
            .field("page", &self.page)
            .field("writes_as_app", &self.writes.is_some())
            .finish_non_exhaustive()
    }
}

/// The app user a tracker writes as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppUser {
    /// The name Linear shows on its comments.
    pub name: String,
}

impl LinearTracker {
    /// What the Linear adapter implements in this build. It files proposals
    /// (OWL-252), which the runner does since OWL-253: the proposals inbox.
    pub const CAPABILITIES: &'static [Capability] = &[
        Capability::ReadTicket,
        Capability::ListAdmitted,
        Capability::Comments,
        Capability::VisibleStage,
        Capability::BlockedBy,
        Capability::ProposalsInbox,
        Capability::Documents,
        Capability::FoldComments,
        Capability::Reactions,
    ];

    /// The adapter over HTTPS with this key.
    pub fn new(key: ApiKey) -> Self {
        Self::with_transport(HttpTransport::new(key))
    }

    /// The adapter over another transport, such as a test replay.
    pub fn with_transport(transport: impl Transport + Send + Sync + 'static) -> Self {
        Self {
            transport: Box::new(transport),
            writes: None,
            app_user: None,
            comment_page: MAX_PAGE,
            page: MAX_PAGE,
        }
    }

    /// The adapter writing its comments and stage moves through `app`, the
    /// transport of the workspace's OAuth app ([`app::AppTransport`] in
    /// production), so they come from its app user and Linear notifies the
    /// people they concern (decision D8); reads stay on the first transport.
    ///
    /// Checked first, with one request through each: `app` must act as an
    /// app user, of the workspace the reads see, that sees the team whose key
    /// is `team`. An app reaches the workspace's public teams only, and app
    /// credentials of another workspace would write to the issue of that
    /// workspace with the same identifier. Refused otherwise, with the
    /// reason; nothing is written either way. The app user's id, read by the
    /// same request, is kept: a state gesture is read past its moves.
    pub fn writing_as_app(
        mut self,
        app: impl Transport + Send + Sync + 'static,
        team: &str,
    ) -> Result<(Self, AppUser), Error> {
        let key = team.to_ascii_uppercase();
        let seen: AppSeen = call(&app, APP_QUERY, json!({ "key": key }))?;
        let ours: Workspace = self.call(WORKSPACE_QUERY, json!({}))?;
        let name = seen.viewer.display_name;
        if !seen.viewer.app {
            return Err(invalid(format!(
                "the Linear app's token acts as {name:?}, which is not an app user"
            )));
        }
        if seen.organization.id != ours.organization.id {
            return Err(invalid(format!(
                "the Linear app {name:?} belongs to another workspace than the Linear API key"
            )));
        }
        if seen.teams.nodes.is_empty() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!(
                    "the Linear app {name:?} does not see team {key:?}: an app reaches the \
                     workspace's public teams only"
                ),
            ));
        }
        self.writes = Some(Box::new(app));
        self.app_user = Some(seen.viewer.id);
        Ok((self, AppUser { name }))
    }

    /// Reads comments `size` at a time (1 to [`MAX_PAGE`], the default), so a
    /// recording can cover pagination on a ticket with few comments.
    pub fn with_comment_page(mut self, size: u32) -> Self {
        self.comment_page = size.clamp(1, MAX_PAGE);
        self
    }

    /// Reads the listing of admitted tickets and an issue's history `size`
    /// items a page (1 to [`MAX_PAGE`], the default), as
    /// [`with_comment_page`](Self::with_comment_page) does for comments.
    pub fn with_page(mut self, size: u32) -> Self {
        self.page = size.clamp(1, MAX_PAGE);
        self
    }

    /// Sends one read and returns its `data`, decoded.
    fn call<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T, Error> {
        call(self.transport.as_ref(), query, variables)
    }

    /// Reads a connection page after page, `variables` taking the cursor as
    /// `after`, until its last page or a page `enough` accepts. `connection`
    /// takes each answer's connection out. A cursor that does not advance is
    /// an error, never a loop.
    fn pages<D: DeserializeOwned, T>(
        &self,
        what: &str,
        query: &str,
        mut variables: Value,
        mut connection: impl FnMut(D) -> Connection<T>,
        enough: impl Fn(&[T]) -> bool,
    ) -> Result<Vec<T>, Error> {
        let mut items = Vec::new();
        let mut after: Option<String> = None;
        loop {
            variables["after"] = json!(after);
            let page = connection(self.call(query, variables.clone())?);
            let stop = enough(&page.nodes);
            items.extend(page.nodes);
            if stop || !page.page_info.has_next_page {
                return Ok(items);
            }
            match page.page_info.end_cursor {
                Some(cursor) if after.as_ref() != Some(&cursor) => after = Some(cursor),
                _ => return Err(invalid(format!("the pages of {what} do not advance"))),
            }
        }
    }

    /// Sends one write, through the app when one is set.
    fn write<T: DeserializeOwned>(&self, query: &str, variables: Value) -> Result<T, Error> {
        let transport = self.writes.as_ref().unwrap_or(&self.transport);
        call(transport.as_ref(), query, variables)
    }
}

/// Sends one request through `transport` and returns its `data`, decoded.
fn call<T: DeserializeOwned>(
    transport: &(dyn Transport + Send + Sync),
    query: &str,
    variables: Value,
) -> Result<T, Error> {
    let request = json!({ "query": query, "variables": variables });
    let response = transport
        .send(&request)
        .map_err(|e| Error::new(ErrorKind::Other, format!("Linear: {e}")))?;
    decode(&response)
}

impl LinearTracker {
    /// The names of the workflow states of the team whose key is `team`, as
    /// Linear lists them: what `owlshift doctor` checks `[tracker].states`
    /// against (OWL-147). Read only. The key is sent upper case, as Linear
    /// writes team keys, since `owlshift do` compares it without regard to
    /// case. No team of that key is [`ErrorKind::NotFound`]; more than
    /// [`MAX_PAGE`] states are refused, as [`Tracker::set_stage`] refuses them.
    pub fn team_states(&self, team: &str) -> Result<Vec<String>, Error> {
        let key = team.to_ascii_uppercase();
        let data: TeamsData = self.call(TEAM_STATES_QUERY, json!({ "key": key }))?;
        let Some(team) = data.teams.nodes.into_iter().next() else {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("Linear has no team with the key {key:?}"),
            ));
        };
        one_page(&team)?;
        Ok(team.states.nodes.into_iter().map(|s| s.name).collect())
    }
}

/// What `owlshift doctor` learns of a team's admission and proposals setup
/// (OWL-155).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionFacts {
    /// Where a proposal would be filed, as `propose` chooses: `Ok(false)`
    /// in the triage inbox, `Ok(true)` in the first backlog state with the
    /// [`PROPOSED_LABEL`], else why it cannot be filed.
    pub inbox: Result<bool, String>,
    /// Of the label names asked, those a team ticket can carry: exactly
    /// named, on the team or on the workspace, not a label group.
    pub labels: Vec<String>,
    /// Of the account ids asked, those Linear knows, deactivated ones
    /// included.
    pub accounts: Vec<AccountFact>,
}

/// One account of the workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountFact {
    pub id: String,
    /// An app user, which never admits.
    pub app: bool,
    /// Not deactivated.
    pub active: bool,
}

impl LinearTracker {
    /// The facts behind doctor's admission lines, read only (checked
    /// 2026-10-14, build plan "P4 · The drain"): the team's states and
    /// triage setting; the labels named `labels`, on the team or the
    /// workspace; the accounts `accounts` by id, deactivated ones included.
    /// `ready` is the project's ready state, which a proposal never goes to.
    /// No team of that key is [`ErrorKind::NotFound`]. More answers than one
    /// page holds are refused, not truncated.
    pub fn admission_facts(
        &self,
        team: &str,
        ready: &str,
        labels: &[&str],
        accounts: &[&str],
    ) -> Result<AdmissionFacts, Error> {
        let key = team.to_ascii_uppercase();
        let data: AdmissionTeamData = self.call(ADMISSION_TEAM_QUERY, json!({ "key": key }))?;
        let Some(found) = data.teams.nodes.into_iter().next() else {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("Linear has no team with the key {key:?}"),
            ));
        };
        let TargetTeam {
            id: team_id,
            triage_enabled,
            team: found,
        } = found;
        one_page(&found)?;
        let inbox = proposals_inbox(triage_enabled, &found.states.nodes, &found.key, ready)
            .map(|(_, labelled)| labelled)
            .map_err(|e| e.message);
        let mut present = Vec::new();
        if !labels.is_empty() {
            let data: AdmissionLabelsData =
                self.call(ADMISSION_LABELS_QUERY, json!({ "names": labels }))?;
            if data.issue_labels.page_info.has_next_page {
                return Err(invalid(format!(
                    "more than {MAX_PAGE} labels are named like {labels:?}, which this adapter \
                     does not read"
                )));
            }
            for label in data.issue_labels.nodes {
                let ours = label.team.as_ref().is_none_or(|t| t.id == team_id);
                if ours && !label.is_group && labels.contains(&label.name.as_str()) {
                    present.push(label.name);
                }
            }
        }
        let mut known = Vec::new();
        if !accounts.is_empty() {
            if accounts.len() > MAX_PAGE as usize {
                return Err(invalid(format!(
                    "{} admitters are listed, more than the {MAX_PAGE} this adapter checks",
                    accounts.len()
                )));
            }
            let data: AdmissionUsersData =
                self.call(ADMISSION_USERS_QUERY, json!({ "ids": accounts }))?;
            known = data
                .users
                .nodes
                .into_iter()
                .filter(|user| accounts.contains(&user.id.as_str()))
                .map(|user| AccountFact {
                    id: user.id,
                    app: user.app,
                    active: user.active,
                })
                .collect();
        }
        Ok(AdmissionFacts {
            inbox,
            labels: present,
            accounts: known,
        })
    }
}

/// Why no single workflow state answers to a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateMismatch {
    /// No state has that name.
    Missing,
    /// Several states have it.
    Ambiguous,
}

/// Where, among a team's workflow state names, the one named `wanted` is.
/// The name must match exactly, case included, and only once: the rule by
/// which the stage moves, and by which `owlshift doctor` checks the
/// project's `[tracker].states`.
pub fn find_state<'a>(
    names: impl IntoIterator<Item = &'a str>,
    wanted: &str,
) -> Result<usize, StateMismatch> {
    let mut found = None;
    for (at, name) in names.into_iter().enumerate() {
        if name == wanted {
            if found.is_some() {
                return Err(StateMismatch::Ambiguous);
            }
            found = Some(at);
        }
    }
    found.ok_or(StateMismatch::Missing)
}

/// A team's workflow states are read in one page, never truncated.
fn one_page<S>(team: &Team<S>) -> Result<(), Error> {
    if team.states.page_info.has_next_page {
        return Err(invalid(format!(
            "team {} has more than {MAX_PAGE} workflow states, which this adapter does not \
             read",
            team.key
        )));
    }
    Ok(())
}

impl Tracker for LinearTracker {
    fn capabilities(&self) -> &'static [Capability] {
        Self::CAPABILITIES
    }

    fn ticket(&self, id: &TicketId) -> Result<Ticket, Error> {
        let data: TicketData = self.call(TICKET_QUERY, json!({ "id": id.as_str() }))?;
        data.issue.into_ticket()
    }

    fn comments(&self, id: &TicketId) -> Result<Vec<Comment>, Error> {
        let variables = json!({ "id": id.as_str(), "first": self.comment_page });
        let mut comments: Vec<Comment> = self
            .pages(
                "comments",
                COMMENTS_QUERY,
                variables,
                |data: CommentsData| data.issue.comments,
                |_| false,
            )?
            .into_iter()
            .map(LinearComment::into_comment)
            .collect();
        // Linear lists newest first; the contract is oldest first.
        comments.sort_by_key(|c| c.created_at);
        Ok(comments)
    }

    /// The team's issues in the state named `ready`, carrying the label for
    /// a label gesture, 50 a page to the last (architecture section 5, "The
    /// ready column"). Their blockers are the `blocks` relations whose
    /// `relatedIssue` is the ticket, archived blockers included (checked
    /// 2026-10-13, above). More than 50 labels or relations on one ticket
    /// is refused, never truncated, as [`Tracker::ticket`] refuses labels.
    fn admitted(&self, column: &Column<'_>) -> Result<Vec<Listed>, Error> {
        let Some(team) = column.team else {
            return Err(invalid(
                "listing the Linear ready column needs the project's `[tracker].team`",
            ));
        };
        let mut filter = json!({
            "team": { "key": { "eq": team.to_ascii_uppercase() } },
            "state": { "name": { "eq": column.ready } },
        });
        match column.admit {
            Admit::Label(label) => filter["labels"] = json!({ "name": { "eq": label } }),
            Admit::State(_) => {}
            Admit::Delegation => return Err(no_delegation()),
        }
        let variables = json!({ "filter": filter, "first": self.page });
        self.pages(
            "the ready column",
            ADMITTED_QUERY,
            variables,
            |data: AdmittedData| data.issues,
            |_| false,
        )?
        .into_iter()
        .map(AdmittedIssue::into_listed)
        .collect()
    }

    /// The issues of the team whose state has the name `state`, as the
    /// ready column's listing reads a ticket (OWL-410).
    fn working(&self, team: Option<&str>, state: &str) -> Result<Vec<Ticket>, Error> {
        let Some(team) = team else {
            return Err(invalid(
                "listing the Linear working state needs the project's `[tracker].team`",
            ));
        };
        let filter = json!({
            "team": { "key": { "eq": team.to_ascii_uppercase() } },
            "state": { "name": { "eq": state } },
        });
        let variables = json!({ "filter": filter, "first": self.page });
        self.pages(
            "the working state",
            WORKING_QUERY,
            variables,
            |data: WorkingData| data.issues,
            |_| false,
        )?
        .into_iter()
        .map(Issue::into_ticket)
        .collect()
    }

    /// The history read newest first, page by page, up to the first page
    /// holding an entry that decides the gesture; two deciding entries of one
    /// time split by a page boundary are not both read, an accepted residual.
    ///
    /// A label gesture is the latest entry that added the label: its actor
    /// by the rule of comments (`author`), so an app user or a bot never
    /// admits. Two latest entries of one time by different authors are
    /// nobody's. No entry: the creator, by the same rule, since Linear
    /// records no entry for a label set at creation, and an issue created
    /// through a person's API key is theirs (architecture section 5, "By
    /// whom"). A state gesture is read by `state_gesture`: any move of
    /// state but Owlshift's own app user's decides it, another app's
    /// included, so the read stops at the page holding one.
    fn gesture(&self, id: &TicketId, admit: &Admit) -> Result<Gesture, Error> {
        let ours = self.app_user.as_deref();
        let decides = |entry: &HistoryEntry| match admit {
            Admit::Label(label) => entry
                .added_labels
                .iter()
                .flatten()
                .any(|added| &added.name == label),
            Admit::State(_) => entry.to_state.is_some() && !entry.by(ours),
            Admit::Delegation => false,
        };
        if *admit == Admit::Delegation {
            return Err(no_delegation());
        }
        let mut creation = None;
        let entries = self.pages(
            "the history",
            GESTURE_QUERY,
            json!({ "id": id.as_str(), "first": self.page }),
            |data: GestureData| {
                let issue = data.issue;
                creation.get_or_insert((
                    issue.created_at,
                    issue.creator,
                    issue.bot_actor,
                    issue.external_user_creator,
                ));
                issue.history
            },
            |page| page.iter().any(decides),
        )?;
        let Some((created_at, creator, bot, external)) = creation else {
            return Err(invalid(format!("Linear answered no history for {id}")));
        };
        let creator = Gesture {
            account: author(creator.map(Actor::into_user), bot, external),
            at: Some(created_at),
        };
        if let Admit::State(ready) = admit {
            return Ok(state_gesture(entries, ready, creator, ours));
        }
        let made = entries.into_iter().filter(decides).collect();
        Ok(match latest(made) {
            Some((at, latest)) => Gesture {
                account: one_author(latest),
                at: Some(at),
            },
            None => creator,
        })
    }

    fn post_comment(&self, id: &TicketId, body: &str) -> Result<Comment, Error> {
        let variables = json!({ "issueId": id.as_str(), "body": body });
        let data: PostData = self.write(POST_MUTATION, variables)?;
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
    /// one named `state`, then `issueUpdate` with its id, through the app
    /// when one is set. The issue is updated by the id Linear gave in the
    /// first answer. Nothing is cached: a command moves the stage a few times
    /// at most.
    fn set_stage(&self, id: &TicketId, state: &str) -> Result<(), Error> {
        let data: StatesData = self.call(STATES_QUERY, json!({ "id": id.as_str() }))?;
        let issue = data.issue;
        let team = issue.team;
        one_page(&team)?;
        let nodes = &team.states.nodes;
        let target = match find_state(nodes.iter().map(|s| s.name.as_str()), state) {
            Ok(at) => &nodes[at],
            Err(StateMismatch::Missing) => {
                let names: Vec<&str> = nodes.iter().map(|s| s.name.as_str()).collect();
                return Err(invalid(format!(
                    "team {} has no workflow state named {state:?}: the project's \
                     `[tracker].states` must name one of {}",
                    team.key,
                    names.join(", ")
                )));
            }
            Err(StateMismatch::Ambiguous) => {
                return Err(invalid(format!(
                    "team {} has several workflow states named {state:?}",
                    team.key
                )));
            }
        };
        let variables = json!({ "id": issue.id, "stateId": target.id });
        let data: StageData = self.write(STAGE_MUTATION, variables)?;
        match data.issue_update {
            IssueUpdate {
                success: true,
                issue: Some(IssueState { state: Some(now) }),
            } if now.id == target.id => Ok(()),
            _ => Err(invalid(format!("Linear did not move {id} to {state:?}"))),
        }
    }

    fn ticket_url(&self, id: &TicketId) -> Option<String> {
        self.issue_url(id).ok()
    }

    /// The issue's workflow state, by its name (OWL-112).
    fn stage(&self, id: &TicketId) -> Result<String, Error> {
        let data: StageNameData = self.call(STAGE_QUERY, json!({ "id": id.as_str() }))?;
        Ok(data.issue.state.name)
    }

    /// The issue's workflow state type (OWL-250). An identifier Linear does
    /// not know answers `Entity not found: Issue`, which is
    /// [`ErrorKind::NotFound`] (checked live on 2026-10-14, OWL-264).
    fn state_type(&self, id: &TicketId) -> Result<Option<StateType>, Error> {
        let data: StateTypeData = self.call(STATE_TYPE_QUERY, json!({ "id": id.as_str() }))?;
        state_type(&data.issue.state.kind).map(Some)
    }

    /// The account's profile link (`User.url`), which Linear turns into a
    /// mention of them (checked 2026-10-04, OWL-157). Read each time, a few
    /// times a ticket: a person may rename their profile while `watch`
    /// runs. An error when it cannot be read, or is not a plain
    /// `https://linear.app/` link that Markdown would leave whole: the
    /// comment then goes without it, and the runner says so (OWL-170).
    fn mention(&self, account: &str) -> Result<Option<String>, Error> {
        let data: UserData = self.call(USER_QUERY, json!({ "id": account }))?;
        profile_link(&data.user.url).map(Some).ok_or_else(|| {
            invalid(format!(
                "the profile link of account {account} is not a plain https://linear.app/ link"
            ))
        })
    }

    /// Reads the parent's team and project, and the label when the team
    /// has no triage inbox, through the key; then writes the issue and its
    /// relations through the app when one is set (checked 2026-10-13,
    /// above). The inbox is the team's first state of type `triage`, by
    /// position, when the team has triage on; else its first `backlog`
    /// state by position with [`PROPOSED_LABEL`], the team's label before a
    /// workspace one. The label is never created: doctor warns of a missing
    /// one (build plan, "P4 · The drain", part D). One relation is written,
    /// by the issues' ids, since Linear keeps one between two issues, a
    /// later write replacing the earlier without a word (checked 2026-10-15,
    /// OWL-292, above): `blocks` from the parent to the proposal, `issue`
    /// blocking `relatedIssue`, when the proposal needs the parent's code;
    /// else `related` from the proposal to the parent.
    fn propose(&self, proposal: &Proposal<'_>) -> Result<Filed, Error> {
        let parent = proposal.parent;
        let data: TargetData = self.call(TARGET_QUERY, json!({ "id": parent.as_str() }))?;
        let target = data.issue;
        let TargetTeam {
            id: team_id,
            triage_enabled,
            team,
        } = target.team;
        one_page(&team)?;
        let (state, labelled) = proposals_inbox(
            triage_enabled,
            &team.states.nodes,
            &team.key,
            proposal.ready,
        )?;
        let mut input = json!({
            "teamId": team_id,
            "stateId": state.id,
            "title": proposal.title,
            "description": proposal.description,
        });
        if let Some(project) = target.project {
            input["projectId"] = json!(project.id);
        }
        if labelled {
            input["labelIds"] = json!([self.proposed_label(&team_id, &team.key)?]);
        }
        let data: CreateData = self.write(PROPOSE_MUTATION, json!({ "input": input }))?;
        let IssueCreate {
            success: true,
            issue: Some(created),
        } = data.issue_create
        else {
            return Err(invalid(format!(
                "Linear did not file the proposal of {parent}"
            )));
        };
        let id = TicketId::new(created.identifier).map_err(|e| invalid(e.to_string()))?;
        let mut problems = Vec::new();
        if created.state.id != state.id {
            problems.push(invalid(format!(
                "Linear filed {id} in {:?}, not in {:?}",
                created.state.name, state.name
            )));
        }
        let (kind, issue, related) = if proposal.blocked_by_parent {
            ("blocks", &target.id, &created.id)
        } else {
            ("related", &created.id, &target.id)
        };
        let input = json!({ "type": kind, "issueId": issue, "relatedIssueId": related });
        let what = format!("the `{kind}` relation between {parent} and {id}");
        match self.write::<RelateData>(RELATE_MUTATION, json!({ "input": input })) {
            Ok(RelateData {
                issue_relation_create: Success { success: true },
            }) => {}
            Ok(_) => problems.push(invalid(format!("Linear did not write {what}"))),
            Err(e) => problems.push(Error::new(e.kind, format!("{what} was not written: {e}"))),
        }
        Ok(Filed {
            id,
            state: created.state.name,
            problems,
        })
    }

    /// Two filters on the team's issues, `includeArchived` set, each read
    /// to its last page: `title: { eqIgnoreCase }` and `description: {
    /// contains }` (checked 2026-10-13, above). The completed ones are left
    /// out here, by their state's type; a type this adapter does not know
    /// is an error, as in the listing.
    fn proposed(&self, team: Option<&str>, title: &str, marker: &str) -> Result<Vec<Found>, Error> {
        let Some(team) = team else {
            return Err(invalid(
                "searching the Linear proposals needs the project's `[tracker].team`",
            ));
        };
        // An empty `contains` matches every issue.
        if title.trim().is_empty() || marker.trim().is_empty() {
            return Err(invalid(
                "a proposal is searched by a title and a marker that are not blank",
            ));
        }
        let mut found: Vec<Found> = Vec::new();
        for mut filter in [
            json!({ "title": { "eqIgnoreCase": title } }),
            json!({ "description": { "contains": marker } }),
        ] {
            filter["team"] = json!({ "key": { "eq": team.to_ascii_uppercase() } });
            let issues = self.pages(
                "the proposals",
                PROPOSED_QUERY,
                json!({ "filter": filter, "first": self.page }),
                |data: ProposedData| data.issues,
                |_| false,
            )?;
            for issue in issues {
                if state_type(&issue.state.kind)? == StateType::Completed {
                    continue;
                }
                let id = TicketId::new(issue.identifier).map_err(|e| invalid(e.to_string()))?;
                if !found.iter().any(|f| f.id == id) {
                    found.push(Found {
                        id,
                        description: issue.description.unwrap_or_default(),
                    });
                }
            }
        }
        Ok(found)
    }

    /// One read through the key: the issue's identifier, which Linear
    /// answers whatever spelling asked it, its state's type, its
    /// description and its blockers, read as the listing reads them. An
    /// unknown issue is [`ErrorKind::NotFound`] (`Entity not found: Issue`,
    /// OWL-264).
    fn relations(&self, id: &TicketId) -> Result<Relations, Error> {
        let data: RelationsData = self.call(RELATIONS_QUERY, json!({ "id": id.as_str() }))?;
        let issue = data.issue;
        let id = TicketId::new(issue.identifier).map_err(|e| invalid(e.to_string()))?;
        let blockers = blockers(&id, issue.inverse_relations)?;
        Ok(Relations {
            id,
            state: Some(state_type(&issue.state.kind)?),
            description: issue.description.unwrap_or_default(),
            blockers,
        })
    }

    /// `issueRelationCreate` of type `blocks`, `blocker` as `issueId` and
    /// `blocked` as `relatedIssueId`, by their identifiers, through the app
    /// when one is set (check (d), 2026-10-14, above): a relation already
    /// there is answered again, not doubled, and a `related` or reverse
    /// `blocks` relation of the pair is replaced (OWL-292, above).
    fn block(&self, blocker: &TicketId, blocked: &TicketId) -> Result<(), Error> {
        let input = json!({ "type": "blocks", "issueId": blocker.as_str(),
            "relatedIssueId": blocked.as_str() });
        let data: RelateData = self.write(RELATE_MUTATION, json!({ "input": input }))?;
        if data.issue_relation_create.success {
            Ok(())
        } else {
            Err(invalid(format!(
                "Linear did not write that {blocker} blocks {blocked}"
            )))
        }
    }

    /// `documentCreate` with the issue's identifier, or `documentUpdate` of
    /// the content alone, through the app when one is set. An update that
    /// answers the document archived, in Linear's trash, is
    /// [`ErrorKind::NotFound`], as an unknown id is (checked 2026-10-19,
    /// above). Content past [`MAX_DOCUMENT_CHARS`] is refused before any
    /// request.
    fn publish_document(
        &self,
        id: &TicketId,
        document: Option<&str>,
        title: &str,
        content: &str,
    ) -> Result<Document, Error> {
        if content.chars().count() > MAX_DOCUMENT_CHARS {
            return Err(invalid(format!(
                "a document of {id} holds at most {MAX_DOCUMENT_CHARS} characters"
            )));
        }
        let written = match document {
            None => {
                let input = json!({ "issueId": id.as_str(), "title": title, "content": content });
                let data: DocumentCreateData =
                    self.write(DOCUMENT_CREATE_MUTATION, json!({ "input": input }))?;
                data.document_create
            }
            Some(document) => {
                let variables = json!({ "id": document, "input": { "content": content } });
                let data: DocumentUpdateData = self.write(DOCUMENT_UPDATE_MUTATION, variables)?;
                data.document_update
            }
        };
        match written {
            DocumentWrite {
                success: true,
                document:
                    Some(LinearDocument {
                        archived_at: Some(_),
                        ..
                    }),
            } => Err(Error::new(
                ErrorKind::NotFound,
                format!("the document of {id} is in Linear's trash"),
            )),
            DocumentWrite {
                success: true,
                document: Some(LinearDocument { id, url, .. }),
            } => Ok(Document { id, url }),
            _ => Err(invalid(format!(
                "Linear did not write the document of {id}"
            ))),
        }
    }

    /// `commentResolve(id)` through the app when one is set, without
    /// `resolvingCommentId`, which Linear answered `INTERNAL_SERVER_ERROR`
    /// whatever comment it named; a reply is refused (`Cannot resolve a
    /// child comment`) and an unknown id is [`ErrorKind::NotFound`] (all
    /// checked 2026-10-19, build plan "P5 · Quality pipeline", part R).
    fn resolve_comment(&self, comment: &str) -> Result<(), Error> {
        let data: ResolveData = self.write(RESOLVE_MUTATION, json!({ "id": comment }))?;
        if data.comment_resolve.success {
            Ok(())
        } else {
            Err(invalid(format!(
                "Linear did not resolve the comment {comment}"
            )))
        }
    }
}

impl LinearTracker {
    /// The id of the [`PROPOSED_LABEL`] a proposal in the backlog of the
    /// team whose id is `team` carries: the team's own, else the
    /// workspace's. Names match exactly, case included, as Linear's `eq`
    /// does (checked 2026-10-13, above).
    fn proposed_label(&self, team: &str, key: &str) -> Result<String, Error> {
        let data: LabelsData = self.call(LABEL_QUERY, json!({ "name": PROPOSED_LABEL }))?;
        let labels = data.issue_labels.nodes;
        let ours = labels
            .iter()
            .find(|label| label.team.as_ref().is_some_and(|t| t.id == team));
        ours.or_else(|| labels.iter().find(|label| label.team.is_none()))
            .map(|label| label.id.clone())
            .ok_or_else(|| {
                invalid(format!(
                    "team {key} has no triage inbox, and neither it nor the workspace has the \
                     label {PROPOSED_LABEL:?} that marks a proposal in its backlog"
                ))
            })
    }
}

/// Where a proposal goes in a team: the first `triage` state by position
/// when the team has triage on, else its first `backlog` state, which then
/// takes [`PROPOSED_LABEL`] (the `bool`). Never the ready state `ready`: a
/// proposal never enters the queue without a person. What `propose` files
/// by and `owlshift doctor` checks (OWL-155).
fn proposals_inbox<'a>(
    triage_enabled: bool,
    states: &'a [TypedState],
    key: &str,
    ready: &str,
) -> Result<(&'a TypedState, bool), Error> {
    let first = |kind: &str| {
        states
            .iter()
            .filter(|state| state.kind == kind)
            .min_by(|a, b| a.position.total_cmp(&b.position))
    };
    let triage = if triage_enabled {
        first("triage")
    } else {
        None
    };
    let (state, labelled) = match (triage, first("backlog")) {
        (Some(state), _) => (state, false),
        (None, Some(state)) => (state, true),
        (None, None) => {
            return Err(invalid(format!(
                "team {key} has no triage inbox and no backlog state to file a proposal in"
            )));
        }
    };
    if state.name == ready {
        return Err(invalid(format!(
            "team {key}'s proposals would go to {:?}, the ready state: a proposal never enters \
             the queue without a person",
            state.name
        )));
    }
    Ok((state, labelled))
}

/// `url` when it is a Linear link with nothing Markdown reads as syntax.
fn profile_link(url: &str) -> Option<String> {
    let plain = url
        .chars()
        .all(|c| !c.is_whitespace() && !c.is_control() && !"()[]<>\"'`\\".contains(c));
    (url.starts_with("https://linear.app/") && plain).then(|| url.to_owned())
}

/// The fields read from every comment. No e-mail, no full name: only what the
/// contract carries, whether the user is an app (OWL-157), and the comment a
/// reply answers (OWL-146: `issue.comments` lists replies too, each naming its
/// `parent`, checked live on 2026-10-11).
macro_rules! comment_fields {
    () => {
        "id body createdAt editedAt user { id displayName app } botActor { name } \
         externalUser { name } parent { id }"
    };
}

/// The fields read from every issue that becomes a [`Ticket`]: a ticket
/// read alone and one read in the listing are the same value.
macro_rules! issue_fields {
    () => {
        "identifier title description priority assignee { id displayName } \
         creator { id displayName } botActor { name } externalUserCreator { name } \
         labels(first: 50) { nodes { name } pageInfo { hasNextPage } }"
    };
}

/// The fields of an account whose `app` decides whether it admits: never
/// left to a default (see [`Actor`]).
macro_rules! actor_fields {
    () => {
        "{ id displayName app }"
    };
}

/// The `blocks` relations whose `relatedIssue` is the issue, archived
/// blockers included (checked 2026-10-13, above), each blocker with the type
/// of its state: the listing's and the dependency check's read alike.
macro_rules! blocker_fields {
    () => {
        "inverseRelations(first: 50, includeArchived: true) { \
         nodes { type issue { identifier state { type } } } pageInfo { hasNextPage } }"
    };
}

const TICKET_QUERY: &str = concat!(
    "query Ticket($id: String!) { issue(id: $id) { ",
    issue_fields!(),
    " } }"
);

/// The ready column (OWL-246). The filter is built by
/// [`Tracker::admitted`].
const ADMITTED_QUERY: &str = concat!(
    "query Admitted($filter: IssueFilter!, $first: Int!, $after: String) { \
     issues(first: $first, after: $after, filter: $filter) { nodes { ",
    issue_fields!(),
    " createdAt ",
    blocker_fields!(),
    " } pageInfo { hasNextPage endCursor } } }"
);

/// The tickets in a state (OWL-410). The filter is built by
/// [`Tracker::working`].
const WORKING_QUERY: &str = concat!(
    "query Working($filter: IssueFilter!, $first: Int!, $after: String) { \
     issues(first: $first, after: $after, filter: $filter) { nodes { ",
    issue_fields!(),
    " } pageInfo { hasNextPage endCursor } } }"
);

/// A ticket as intake's dependency check reads it (OWL-249).
const RELATIONS_QUERY: &str = concat!(
    "query Relations($id: String!) { issue(id: $id) { identifier description state { type } ",
    blocker_fields!(),
    " } }"
);

/// An issue's creation and history, for the admission gesture (OWL-246).
const GESTURE_QUERY: &str = concat!(
    "query Gesture($id: String!, $first: Int!, $after: String) { issue(id: $id) { \
     createdAt creator ",
    actor_fields!(),
    " botActor { name } externalUserCreator { name } \
     history(first: $first, after: $after) { nodes { createdAt actor ",
    actor_fields!(),
    " botActor { name } addedLabels { name } fromState { name } toState { name } } \
     pageInfo { hasNextPage endCursor } } } }"
);

/// A ticket's comments, each with its reactions (OWL-371): a comment just
/// posted has none, so the post's mutation does not ask for them.
const COMMENTS_QUERY: &str = concat!(
    "query Comments($id: String!, $first: Int!, $after: String) { issue(id: $id) { \
     comments(first: $first, after: $after) { nodes { ",
    comment_fields!(),
    " reactions { emoji createdAt user { id displayName app } externalUser { name } }",
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

const TEAM_STATES_QUERY: &str = "query TeamStates($key: String!) { \
    teams(filter: { key: { eq: $key } }) { \
    nodes { key states(first: 50) { nodes { name } pageInfo { hasNextPage } } } } }";

const STAGE_MUTATION: &str = "mutation SetStage($id: String!, $stateId: String!) { \
    issueUpdate(id: $id, input: { stateId: $stateId }) { success issue { state { id name } } } }";

/// What [`LinearTracker::writing_as_app`] asks through the app.
const APP_QUERY: &str = "query App($key: String!) { viewer { id displayName app } \
    organization { id } teams(filter: { key: { eq: $key } }) { nodes { id } } }";

/// What [`LinearTracker::writing_as_app`] asks through the API key.
const WORKSPACE_QUERY: &str = "query Workspace { organization { id } }";

/// A mention's link (OWL-157).
const USER_QUERY: &str = "query Mention($id: String!) { user(id: $id) { url } }";

/// Where a proposal of the parent goes (OWL-252).
const TARGET_QUERY: &str = "query ProposalTarget($id: String!) { issue(id: $id) { id \
    project { id } team { id key triageEnabled states(first: 50) { \
    nodes { id name type position } pageInfo { hasNextPage } } } } }";

const LABEL_QUERY: &str = "query ProposedLabel($name: String!) { \
    issueLabels(first: 50, filter: { name: { eq: $name } }) { nodes { id team { id } } } }";

const ADMISSION_TEAM_QUERY: &str = "query AdmissionTeam($key: String!) { \
    teams(filter: { key: { eq: $key } }) { nodes { id key triageEnabled states(first: 50) { \
    nodes { id name type position } pageInfo { hasNextPage } } } } }";

const ADMISSION_LABELS_QUERY: &str = "query AdmissionLabels($names: [String!]) { \
    issueLabels(first: 50, filter: { name: { in: $names } }) { \
    nodes { name isGroup team { id } } pageInfo { hasNextPage } } }";

const ADMISSION_USERS_QUERY: &str = "query AdmissionUsers($ids: [ID!]) { \
    users(first: 50, includeDisabled: true, filter: { id: { in: $ids } }) { \
    nodes { id active app } } }";

const PROPOSE_MUTATION: &str = "mutation Propose($input: IssueCreateInput!) { \
    issueCreate(input: $input) { success issue { id identifier state { id name } } } }";

/// A ticket's document, made (OWL-350).
const DOCUMENT_CREATE_MUTATION: &str = "mutation CreateDocument($input: DocumentCreateInput!) { \
    documentCreate(input: $input) { success document { id url archivedAt } } }";

/// A ticket's document, its content replaced (OWL-350).
const DOCUMENT_UPDATE_MUTATION: &str = "mutation UpdateDocument($id: String!, \
    $input: DocumentUpdateInput!) { documentUpdate(id: $id, input: $input) { \
    success document { id url archivedAt } } }";

/// A comment resolved, so Linear shows it folded (OWL-353).
const RESOLVE_MUTATION: &str =
    "mutation Resolve($id: String!) { commentResolve(id: $id) { success } }";

const RELATE_MUTATION: &str = "mutation Relate($input: IssueRelationCreateInput!) { \
    issueRelationCreate(input: $input) { success } }";

/// One of the two exact searches (OWL-252); the filter is built by
/// [`Tracker::proposed`].
const PROPOSED_QUERY: &str = "query Proposed($filter: IssueFilter!, $first: Int!, \
    $after: String) { issues(first: $first, after: $after, filter: $filter, \
    includeArchived: true) { nodes { identifier description state { type } } \
    pageInfo { hasNextPage endCursor } } }";

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

impl Issue {
    fn into_ticket(self) -> Result<Ticket, Error> {
        let id = TicketId::new(self.identifier).map_err(|e| invalid(e.to_string()))?;
        if self.labels.page_info.has_next_page {
            return Err(invalid(format!(
                "{id} has more than {MAX_PAGE} labels, which this adapter does not read"
            )));
        }
        Ok(Ticket {
            id,
            title: self.title,
            description: self.description.unwrap_or_default(),
            priority: priority(self.priority)?,
            assignee: self.assignee.map(User::into_person),
            labels: self.labels.nodes.into_iter().map(|l| l.name).collect(),
            author: ticket_author(self.creator, self.bot_actor, self.external_user_creator),
        })
    }
}

#[derive(Deserialize)]
struct WorkingData {
    issues: Connection<Issue>,
}

#[derive(Deserialize)]
struct AdmittedData {
    issues: Connection<AdmittedIssue>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdmittedIssue {
    #[serde(flatten)]
    issue: Issue,
    created_at: Timestamp,
    inverse_relations: Connection<Relation>,
}

#[derive(Deserialize)]
struct Relation {
    #[serde(rename = "type")]
    kind: String,
    /// The blocker for a `blocks` relation. Never expected null: a relation
    /// Linear answers without its issue is refused rather than dropped,
    /// since a dropped blocker would release the ticket.
    issue: Option<RelatedIssue>,
}

#[derive(Deserialize)]
struct RelatedIssue {
    identifier: String,
    state: StateOfType,
}

#[derive(Deserialize)]
struct StateOfType {
    #[serde(rename = "type")]
    kind: String,
}

impl AdmittedIssue {
    fn into_listed(self) -> Result<Listed, Error> {
        let ticket = self.issue.into_ticket()?;
        let blockers = blockers(&ticket.id, self.inverse_relations)?;
        Ok(Listed {
            ticket,
            created_at: Some(self.created_at),
            blockers,
        })
    }
}

/// The blockers of the issue `id`, each once, from its `inverseRelations`.
/// More relations than a page, a relation without its issue and a state
/// type this adapter does not know are refused, never dropped: a dropped
/// blocker would release the ticket.
fn blockers(id: &TicketId, relations: Connection<Relation>) -> Result<Vec<Blocker>, Error> {
    if relations.page_info.has_next_page {
        return Err(invalid(format!(
            "{id} has more than {MAX_PAGE} relations, which this adapter does not read"
        )));
    }
    let mut blockers: Vec<Blocker> = Vec::new();
    for relation in relations.nodes {
        if relation.kind != "blocks" {
            continue;
        }
        let Some(blocker) = relation.issue else {
            return Err(invalid(format!(
                "Linear answered a blocker of {id} without its issue"
            )));
        };
        let blocker = Blocker {
            id: TicketId::new(blocker.identifier).map_err(|e| invalid(e.to_string()))?,
            state: Some(state_type(&blocker.state.kind)?),
        };
        if !blockers.contains(&blocker) {
            blockers.push(blocker);
        }
    }
    Ok(blockers)
}

#[derive(Deserialize)]
struct RelationsData {
    issue: RelationsIssue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelationsIssue {
    identifier: String,
    description: Option<String>,
    state: StateOfType,
    inverse_relations: Connection<Relation>,
}

/// Linear's `WorkflowState.type`. A type this adapter does not know is
/// refused: guessing could release a ticket.
fn state_type(kind: &str) -> Result<StateType, Error> {
    Ok(match kind {
        "triage" => StateType::Triage,
        "backlog" => StateType::Backlog,
        "unstarted" => StateType::Unstarted,
        "started" => StateType::Started,
        "completed" => StateType::Completed,
        "canceled" => StateType::Canceled,
        "duplicate" => StateType::Duplicate,
        other => return Err(invalid(format!("unknown Linear state type {other:?}"))),
    })
}

#[derive(Deserialize)]
struct GestureData {
    issue: GestureIssue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GestureIssue {
    created_at: Timestamp,
    creator: Option<Actor>,
    bot_actor: Option<Named>,
    external_user_creator: Option<Named>,
    history: Connection<HistoryEntry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryEntry {
    created_at: Timestamp,
    actor: Option<Actor>,
    bot_actor: Option<Named>,
    /// Null when the entry added none.
    added_labels: Option<Vec<Label>>,
    /// Read only for the oldest move, to tell whether the issue was created
    /// in the ready state ([`state_gesture`]).
    from_state: Option<StateName>,
    to_state: Option<StateName>,
}

impl HistoryEntry {
    /// Made by the app user whose id is `app_user`, as the runner's own
    /// moves are through the app; never when there is none. The actor must
    /// also say it is an app, so a person's entry is never taken for the
    /// app's.
    fn by(&self, app_user: Option<&str>) -> bool {
        self.actor
            .as_ref()
            .is_some_and(|actor| actor.app && Some(actor.id.as_str()) == app_user)
    }

    fn moved_to(&self, state: &str) -> bool {
        self.to_state.as_ref().is_some_and(|s| s.name == state)
    }
}

/// The entries of the latest time among `entries`, with that time.
fn latest(mut entries: Vec<HistoryEntry>) -> Option<(Timestamp, Vec<HistoryEntry>)> {
    let at = entries.iter().map(|entry| entry.created_at).max()?;
    entries.retain(|entry| entry.created_at == at);
    Some((at, entries))
}

/// The one author of `entries`, by the rule of comments (`author`), or
/// nobody when they differ.
fn one_author(entries: Vec<HistoryEntry>) -> Author {
    let mut authors = entries
        .into_iter()
        .map(|entry| author(entry.actor.map(Actor::into_user), entry.bot_actor, None));
    let first = authors.next().unwrap_or_else(Author::unknown);
    if authors.all(|other| other == first) {
        first
    } else {
        Author::unknown()
    }
}

/// A state gesture (architecture section 5, "By whom"), from the history
/// read and the creator's gesture. The moves of state Owlshift's own app
/// user made, whose id is `app_user`, are skipped: the runner moves a ticket
/// back to `ready` through the app ("Back to the ready column"), and an app
/// never admits. Another app's moves are not (OWL-270). The latest move left
/// decides: to `ready`, its author is the gesture, which an app's never
/// admits; anywhere else, the ticket was taken out of the column, and the
/// gesture is nobody's. No move left: the creator when the issue was created
/// in `ready`, which the oldest move's `fromState` tells, or no move at all;
/// otherwise nobody's.
fn state_gesture(
    entries: Vec<HistoryEntry>,
    ready: &str,
    creator: Gesture,
    app_user: Option<&str>,
) -> Gesture {
    let (moves, by_apps): (Vec<HistoryEntry>, Vec<HistoryEntry>) = entries
        .into_iter()
        .filter(|entry| entry.to_state.is_some())
        .partition(|entry| !entry.by(app_user));
    if let Some((at, latest)) = latest(moves) {
        let account = if latest.iter().all(|entry| entry.moved_to(ready)) {
            one_author(latest)
        } else {
            Author::unknown()
        };
        return Gesture {
            account,
            at: Some(at),
        };
    }
    let oldest = by_apps.iter().map(|entry| entry.created_at).min();
    let created_ready = by_apps
        .iter()
        .filter(|entry| Some(entry.created_at) == oldest)
        .all(|entry| entry.from_state.as_ref().is_some_and(|s| s.name == ready));
    if created_ready {
        creator
    } else {
        Gesture {
            account: Author::unknown(),
            ..creator
        }
    }
}

/// An account whose gesture may admit. Unlike [`User`], `app` has no
/// default: an answer without it is refused, so an app user can never read
/// as an account because a query stopped asking.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Actor {
    id: String,
    display_name: String,
    app: bool,
}

impl Actor {
    fn into_user(self) -> User {
        User {
            id: self.id,
            display_name: self.display_name,
            app: self.app,
        }
    }
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
    /// An app user, such as Owlshift's own (OWL-157); read on comments only.
    #[serde(default)]
    app: bool,
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
/// too: a limit accepted for comments, which are how the decider answers. An
/// app user, Owlshift's own or another integration's, is never an account: no
/// person holds it (OWL-157).
fn author(user: Option<User>, bot: Option<Named>, external: Option<Named>) -> Author {
    match (user, bot, external) {
        (Some(user), None, None) if !user.app => Author::Account(user.into_person()),
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
    /// Absent from recordings made before OWL-146.
    #[serde(default)]
    parent: Option<Parent>,
    /// Absent from a posted comment's answer, which asks for none, and from
    /// recordings made before OWL-371.
    #[serde(default)]
    reactions: Vec<LinearReaction>,
}

#[derive(Deserialize)]
struct Parent {
    id: String,
}

/// A reaction as the comments query reads it (OWL-371). Linear's
/// `Reaction` has no `botActor` (checked 2026-10-19).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinearReaction {
    emoji: String,
    created_at: Timestamp,
    user: Option<User>,
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
            parent: self.parent.map(|parent| parent.id),
            reactions: self
                .reactions
                .into_iter()
                .map(|reaction| Reaction {
                    emoji: reaction.emoji,
                    // By the rule of comments: an app user's reaction is
                    // never an account's.
                    author: author(reaction.user, None, reaction.external_user),
                    created_at: reaction.created_at,
                })
                .collect(),
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
#[serde(rename_all = "camelCase")]
struct DocumentCreateData {
    document_create: DocumentWrite,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DocumentUpdateData {
    document_update: DocumentWrite,
}

#[derive(Deserialize)]
struct DocumentWrite {
    success: bool,
    document: Option<LinearDocument>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinearDocument {
    id: String,
    url: Option<String>,
    archived_at: Option<String>,
}

#[derive(Deserialize)]
struct StatesData {
    issue: IssueTeam,
}

#[derive(Deserialize)]
struct IssueTeam {
    id: String,
    team: Team<WorkflowState>,
}

#[derive(Deserialize)]
struct Team<S> {
    key: String,
    states: Connection<S>,
}

#[derive(Deserialize)]
struct TeamsData {
    teams: Teams<Team<StateName>>,
}

#[derive(Deserialize)]
struct Teams<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct StateName {
    name: String,
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

/// An issue's link, read for a notification only (OWL-140).
const URL_QUERY: &str = "query TicketUrl($id: String!) { issue(id: $id) { url } }";

const STAGE_QUERY: &str = "query TicketStage($id: String!) { issue(id: $id) { state { name } } }";

const STATE_TYPE_QUERY: &str =
    "query TicketStateType($id: String!) { issue(id: $id) { state { type } } }";

impl LinearTracker {
    /// The link Linear gives an issue, as Linear gives it: whoever shows it
    /// checks it is fit to show.
    fn issue_url(&self, id: &TicketId) -> Result<String, Error> {
        let data: UrlData = self.call(URL_QUERY, json!({ "id": id.as_str() }))?;
        Ok(data.issue.url)
    }
}

#[derive(Deserialize)]
struct UrlData {
    issue: IssueUrl,
}

#[derive(Deserialize)]
struct UserData {
    user: UserUrl,
}

#[derive(Deserialize)]
struct UserUrl {
    url: String,
}

#[derive(Deserialize)]
struct AppSeen {
    viewer: AppViewer,
    organization: Organization,
    teams: Teams<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppViewer {
    id: String,
    display_name: String,
    app: bool,
}

#[derive(Deserialize)]
struct Workspace {
    organization: Organization,
}

#[derive(Deserialize)]
struct Organization {
    id: String,
}

#[derive(Deserialize)]
struct IssueUrl {
    url: String,
}

#[derive(Deserialize)]
struct StageNameData {
    issue: IssueStageName,
}

#[derive(Deserialize)]
struct StateTypeData {
    issue: IssueStateType,
}

#[derive(Deserialize)]
struct IssueStateType {
    state: StateOfType,
}

#[derive(Deserialize)]
struct TargetData {
    issue: Target,
}

#[derive(Deserialize)]
struct Target {
    id: String,
    project: Option<Id>,
    team: TargetTeam,
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetTeam {
    id: String,
    triage_enabled: bool,
    #[serde(flatten)]
    team: Team<TypedState>,
}

#[derive(Deserialize)]
struct TypedState {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    position: f64,
}

#[derive(Deserialize)]
struct AdmissionTeamData {
    teams: Teams<TargetTeam>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdmissionLabelsData {
    issue_labels: Connection<AdmissionLabel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdmissionLabel {
    name: String,
    is_group: bool,
    team: Option<Id>,
}

#[derive(Deserialize)]
struct AdmissionUsersData {
    users: Teams<AdmissionUser>,
}

#[derive(Deserialize)]
struct AdmissionUser {
    id: String,
    active: bool,
    app: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelsData {
    issue_labels: Teams<TeamLabel>,
}

#[derive(Deserialize)]
struct TeamLabel {
    id: String,
    /// Null for a workspace label.
    team: Option<Id>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateData {
    issue_create: IssueCreate,
}

#[derive(Deserialize)]
struct IssueCreate {
    success: bool,
    issue: Option<Created>,
}

#[derive(Deserialize)]
struct Created {
    id: String,
    identifier: String,
    state: WorkflowState,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RelateData {
    issue_relation_create: Success,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveData {
    comment_resolve: Success,
}

#[derive(Deserialize)]
struct Success {
    success: bool,
}

#[derive(Deserialize)]
struct ProposedData {
    issues: Connection<ProposedIssue>,
}

#[derive(Deserialize)]
struct ProposedIssue {
    identifier: String,
    description: Option<String>,
    state: StateOfType,
}

#[derive(Deserialize)]
struct IssueStageName {
    state: StateName,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(status: u16, body: &str) -> Response {
        Response {
            status,
            body: body.to_owned(),
            ..Default::default()
        }
    }

    fn error_kind(status: u16, body: &str) -> ErrorKind {
        decode::<Value>(&answer(status, body)).unwrap_err().kind
    }

    /// OWL-371: a comment's reactions, shaped as Linear answered them on
    /// 2026-10-19, each author by the rule of comments: a person's account,
    /// an app user's (Owlshift's own or another's) never an account, an
    /// external user's neither; a posted comment's answer has none.
    #[test]
    fn reactions_are_read_with_their_authors() {
        let comment: LinearComment = serde_json::from_value(json!({
            "id": "c1", "body": "[owlshift] QUESTIONS · round 1", "createdAt": "2026-10-19T20:07:00.000Z",
            "editedAt": null, "botActor": null, "externalUser": null, "parent": null,
            "user": { "id": "app-1", "displayName": "Owlshift", "app": true },
            "reactions": [
                { "emoji": "+1", "createdAt": "2026-10-19T20:07:35.000Z", "externalUser": null,
                  "user": { "id": "u1", "displayName": "person-1", "app": false } },
                { "emoji": "+1", "createdAt": "2026-10-19T20:08:00.000Z", "externalUser": null,
                  "user": { "id": "app-2", "displayName": "Other app", "app": true } },
                { "emoji": "heart", "createdAt": "2026-10-19T20:09:00.000Z", "user": null,
                  "externalUser": { "name": "Slack user" } }
            ]
        }))
        .unwrap();
        let comment = comment.into_comment();
        let at = |s: &str| -> Timestamp { s.parse().unwrap() };
        assert_eq!(
            comment.reactions,
            [
                Reaction {
                    emoji: "+1".into(),
                    author: Author::Account(Person {
                        id: "u1".into(),
                        name: "person-1".into(),
                    }),
                    created_at: at("2026-10-19T20:07:35Z"),
                },
                Reaction {
                    emoji: "+1".into(),
                    author: Author::Other {
                        name: "Other app".into(),
                    },
                    created_at: at("2026-10-19T20:08:00Z"),
                },
                Reaction {
                    emoji: "heart".into(),
                    author: Author::Other {
                        name: "Slack user".into(),
                    },
                    created_at: at("2026-10-19T20:09:00Z"),
                },
            ]
        );
        assert!(comment.reactions[0].is_thumbs_up() && !comment.reactions[2].is_thumbs_up());
        let posted: LinearComment = serde_json::from_value(json!({
            "id": "c2", "body": "b", "createdAt": "2026-10-19T20:10:00.000Z", "editedAt": null,
            "botActor": null, "externalUser": null, "parent": null,
            "user": { "id": "app-1", "displayName": "Owlshift", "app": true }
        }))
        .unwrap();
        assert!(posted.into_comment().reactions.is_empty());
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
            parent: None,
            reactions: Vec::new(),
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

    /// Owlshift's own comment as Linear answered it on 2026-10-04 (OWL-157),
    /// posted through the app: a user with `app` true and nothing beside
    /// it. An app user is never an account, so never the decider.
    #[test]
    fn an_app_users_comment_is_never_an_accounts() {
        let page = r#"{"data":{"issue":{"comments":{"nodes":[
            {"id":"c2","body":"b","createdAt":"2026-10-04T21:01:00Z","editedAt":null,
             "user":{"id":"u1","displayName":"person-1","app":false},
             "botActor":null,"externalUser":null},
            {"id":"c1","body":"b","createdAt":"2026-10-04T21:00:00Z","editedAt":null,
             "user":{"id":"u2","displayName":"owlshiftbot","app":true},
             "botActor":null,"externalUser":null}],
            "pageInfo":{"hasNextPage":false,"endCursor":null}}}}}"#;
        let comments = tracker(vec![page])
            .comments(&TicketId::new("OWL-1").unwrap())
            .unwrap();
        assert_eq!(
            comments[0].author,
            Author::Other {
                name: "owlshiftbot".to_owned()
            }
        );
        assert!(
            matches!(&comments[1].author, Author::Account(person) if person.id == "u1"),
            "{comments:?}"
        );
    }

    /// A reply as Linear answered it on 2026-10-11 (OWL-146): listed among
    /// the issue's comments, naming its `parent`; a top-level comment's is
    /// `null`.
    #[test]
    fn a_reply_names_the_comment_it_answers() {
        let page = r#"{"data":{"issue":{"comments":{"nodes":[
            {"id":"c2","body":"b","createdAt":"2026-10-11T20:24:31Z","editedAt":null,
             "user":{"id":"u1","displayName":"person-1","app":false},
             "botActor":null,"externalUser":null,"parent":{"id":"c1"}},
            {"id":"c1","body":"b","createdAt":"2026-10-04T09:15:59Z","editedAt":null,
             "user":{"id":"u2","displayName":"owlshiftbot","app":true},
             "botActor":null,"externalUser":null,"parent":null}],
            "pageInfo":{"hasNextPage":false,"endCursor":null}}}}}"#;
        let comments = tracker(vec![page])
            .comments(&TicketId::new("OWL-1").unwrap())
            .unwrap();
        let parents: Vec<Option<&str>> = comments.iter().map(|c| c.parent.as_deref()).collect();
        assert_eq!(parents, [None, Some("c1")]);
    }

    /// Answers each request by its operation's name, and keeps the names and
    /// the variables. Several answers to one name are given in order, the
    /// last one again and again.
    struct Scripted {
        answers: std::sync::Mutex<Vec<(&'static str, Value)>>,
        asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        sent: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl Scripted {
        fn new(answers: Vec<(&'static str, Value)>) -> Self {
            Self {
                answers: std::sync::Mutex::new(answers),
                asked: Default::default(),
                sent: Default::default(),
            }
        }
    }

    impl Transport for Scripted {
        fn send(&self, request: &Value) -> Result<Response, String> {
            let query = request["query"].as_str().unwrap();
            let name = query.split([' ', '(']).nth(1).unwrap().to_owned();
            let mut answers = self.answers.lock().unwrap();
            let mut ours = answers.iter().enumerate().filter(|(_, (n, _))| *n == name);
            let (at, (_, data)) = ours.next().unwrap_or_else(|| panic!("unexpected {name}"));
            let data = data.clone();
            if ours.next().is_some() {
                answers.remove(at);
            }
            self.asked.lock().unwrap().push(name);
            self.sent.lock().unwrap().push(request["variables"].clone());
            Ok(answer(200, &json!({ "data": data }).to_string()))
        }
    }

    /// A parent in a team with these states (`[name, type, position]`),
    /// triage on or off, in a project.
    fn target(triage_enabled: bool, states: &[(&str, &str, f64)]) -> Value {
        let nodes: Vec<Value> = states
            .iter()
            .map(|(name, kind, position)| {
                json!({ "id": format!("s-{name}"), "name": name, "type": kind,
                    "position": position })
            })
            .collect();
        json!({ "issue": { "id": "parent-id", "project": { "id": "p4" },
            "team": { "id": "team-owl", "key": "OWL", "triageEnabled": triage_enabled,
                "states": { "nodes": nodes, "pageInfo": { "hasNextPage": false } } } } })
    }

    /// `issueCreate` as check (e) answered it, in the state `state`.
    fn created(state: &str) -> Value {
        json!({ "issueCreate": { "success": true, "issue": { "id": "new-id",
            "identifier": "OWL-266", "state": { "id": format!("s-{state}"), "name": state } } } })
    }

    fn related(success: bool) -> Value {
        json!({ "issueRelationCreate": { "success": success } })
    }

    /// Team `OWL`'s states as Linear listed them on 2026-10-13, by type.
    const OWL_TYPED: &[(&str, &str, f64)] = &[
        ("In Review", "started", 2.6),
        ("Triage", "triage", 0.0),
        ("Backlog", "backlog", 0.0),
        ("Todo", "unstarted", 1.0),
        ("Done", "completed", 3.0),
        ("Canceled", "canceled", 4.0),
    ];

    fn proposal<'a>(parent: &'a TicketId, blocked_by_parent: bool) -> Proposal<'a> {
        Proposal {
            parent,
            title: "Cap the retries",
            description: "Why.\n\n[owlshift] PROPOSAL · OWL-252 · fingerprint 0123456789ab",
            blocked_by_parent,
            ready: "Todo",
        }
    }

    /// Filing into Triage, as check (e) did: the parent's team and project
    /// read through the key, the issue and its one relation, the parent's
    /// `blocks` (OWL-292), written through the app, by ids, with no label,
    /// priority or estimate.
    #[test]
    fn a_proposal_goes_to_triage_in_the_parents_project_blocked_by_it() {
        let key = Scripted::new(vec![
            ("Workspace", json!({ "organization": { "id": "org-1" } })),
            ("ProposalTarget", target(true, OWL_TYPED)),
        ]);
        let app = Scripted::new(vec![
            ("App", app_seen(true, "org-1", 1)),
            ("Propose", created("Triage")),
            ("Relate", related(true)),
        ]);
        let (by_key, by_app, sent) = (key.asked.clone(), app.asked.clone(), app.sent.clone());
        let (tracker, _) = LinearTracker::with_transport(key)
            .writing_as_app(app, "OWL")
            .unwrap();
        let parent = TicketId::new("OWL-252").unwrap();
        let filed = tracker.propose(&proposal(&parent, true)).unwrap();
        assert_eq!(
            filed,
            Filed {
                id: TicketId::new("OWL-266").unwrap(),
                state: "Triage".to_owned(),
                problems: vec![],
            }
        );
        assert_eq!(*by_key.lock().unwrap(), ["Workspace", "ProposalTarget"]);
        assert_eq!(*by_app.lock().unwrap(), ["App", "Propose", "Relate"]);
        let sent = sent.lock().unwrap();
        let p = proposal(&parent, true);
        assert_eq!(
            sent[1]["input"],
            json!({ "teamId": "team-owl", "stateId": "s-Triage", "projectId": "p4",
                "title": p.title, "description": p.description })
        );
        assert_eq!(
            sent[2]["input"],
            json!({ "type": "blocks", "issueId": "parent-id", "relatedIssueId": "new-id" })
        );
    }

    /// Without a triage inbox (no triage state, or triage off): the first
    /// backlog state by position, with the team's label before the
    /// workspace's, and never another team's.
    #[test]
    fn without_triage_a_proposal_goes_to_the_first_backlog_with_the_label() {
        let parent = TicketId::new("OWL-252").unwrap();
        let backlogs = [
            ("Icebox", "backlog", 1.0),
            ("Backlog", "backlog", 0.5),
            ("Triage", "triage", 0.0),
            ("Todo", "unstarted", 2.0),
        ];
        let label = |id: &str, team: Option<&str>| json!({ "id": id, "team": team.map(|t| json!({ "id": t })) });
        for (states, enabled, labels, chosen) in [
            (
                &backlogs[..],
                false,
                vec![
                    label("other", Some("team-x")),
                    label("ws", None),
                    label("ours", Some("team-owl")),
                ],
                "ours",
            ),
            (
                &backlogs[..2],
                true,
                vec![label("other", Some("team-x")), label("ws", None)],
                "ws",
            ),
        ] {
            let key = Scripted::new(vec![
                ("ProposalTarget", target(enabled, states)),
                (
                    "ProposedLabel",
                    json!({ "issueLabels": { "nodes": labels } }),
                ),
                ("Propose", created("Backlog")),
                ("Relate", related(true)),
            ]);
            let sent = key.sent.clone();
            let filed = LinearTracker::with_transport(key)
                .propose(&proposal(&parent, false))
                .unwrap();
            assert_eq!(filed.state, "Backlog");
            let sent = sent.lock().unwrap();
            assert_eq!(sent[1], json!({ "name": "agent:proposed" }));
            assert_eq!(sent[2]["input"]["stateId"], "s-Backlog");
            assert_eq!(sent[2]["input"]["labelIds"], json!([chosen]));
            assert_eq!(sent.len(), 4, "one relation only: {sent:?}");
            assert_eq!(
                sent[3]["input"],
                json!({ "type": "related", "issueId": "new-id", "relatedIssueId": "parent-id" })
            );
        }
    }

    /// What cannot be filed as the design says is refused before anything
    /// is written, and a refused creation is an error.
    #[test]
    fn a_proposal_that_cannot_go_to_the_inbox_is_refused_unwritten() {
        let parent = TicketId::new("OWL-252").unwrap();
        let no_label =
            json!({ "issueLabels": { "nodes": [{ "id": "x", "team": { "id": "team-x" } }] } });
        let refused = json!({ "issueCreate": { "success": false, "issue": null } });
        for (answers, says) in [
            (
                vec![(
                    "ProposalTarget",
                    target(true, &[("Todo", "unstarted", 0.0)]),
                )],
                "no triage inbox and no backlog",
            ),
            (
                vec![
                    (
                        "ProposalTarget",
                        target(false, &[("Backlog", "backlog", 0.0)]),
                    ),
                    ("ProposedLabel", no_label),
                ],
                "label \"agent:proposed\"",
            ),
            (
                vec![("ProposalTarget", target(false, &[("Todo", "backlog", 0.0)]))],
                "the ready state",
            ),
            (
                vec![
                    ("ProposalTarget", target(true, OWL_TYPED)),
                    ("Propose", refused),
                ],
                "did not file",
            ),
        ] {
            let key = Scripted::new(answers);
            let asked = key.asked.clone();
            let error = LinearTracker::with_transport(key)
                .propose(&proposal(&parent, true))
                .unwrap_err();
            assert!(error.message.contains(says), "{error}");
            assert!(!asked.lock().unwrap().iter().any(|n| n == "Relate"));
        }
    }

    /// Once the issue exists, its relation refused or failed, or another
    /// state than the one asked, is a problem of the filed proposal, never
    /// an error, and no other relation is written in its place.
    #[test]
    fn what_goes_wrong_after_the_creation_is_kept_with_the_filed_proposal() {
        let parent = TicketId::new("OWL-252").unwrap();
        for (relate, says) in [
            (
                related(false),
                "Linear did not write the `blocks` relation between OWL-252 and OWL-266",
            ),
            (
                Value::Null,
                "the `blocks` relation between OWL-252 and OWL-266 was not written: ",
            ),
        ] {
            let key = Scripted::new(vec![
                ("ProposalTarget", target(true, OWL_TYPED)),
                ("Propose", created("Backlog")),
                ("Relate", relate),
            ]);
            let asked = key.asked.clone();
            let filed = LinearTracker::with_transport(key)
                .propose(&proposal(&parent, true))
                .unwrap();
            assert_eq!(filed.id.as_str(), "OWL-266");
            assert_eq!(filed.state, "Backlog");
            let problems: Vec<&str> = filed.problems.iter().map(|e| e.message.as_str()).collect();
            assert_eq!(problems.len(), 2, "{problems:?}");
            assert_eq!(
                problems[0],
                "Linear filed OWL-266 in \"Backlog\", not in \"Triage\""
            );
            assert!(problems[1].starts_with(says), "{problems:?}");
            assert_eq!(
                *asked.lock().unwrap(),
                ["ProposalTarget", "Propose", "Relate"]
            );
        }
    }

    /// The two exact searches: the team's key, the title in any case, the
    /// marker in the description, archived issues included; each issue once,
    /// the completed ones left out.
    #[test]
    fn a_proposal_is_found_by_its_title_or_its_marker_unless_completed() {
        let issue = |id: &str, kind: &str| json!({ "identifier": id, "description": format!("d {id}"), "state": { "type": kind } });
        let page = |nodes: Vec<Value>| {
            json!({ "issues": { "nodes": nodes,
                "pageInfo": { "hasNextPage": false, "endCursor": null } } })
        };
        let key = Scripted::new(vec![
            (
                "Proposed",
                page(vec![issue("OWL-1", "triage"), issue("OWL-2", "completed")]),
            ),
            (
                "Proposed",
                page(vec![issue("OWL-3", "canceled"), issue("OWL-1", "triage")]),
            ),
        ]);
        let sent = key.sent.clone();
        let found = LinearTracker::with_transport(key)
            .proposed(Some("owl"), "Cap the retries", "fingerprint 0123456789ab")
            .unwrap();
        assert_eq!(
            found,
            [
                Found {
                    id: TicketId::new("OWL-1").unwrap(),
                    description: "d OWL-1".to_owned()
                },
                Found {
                    id: TicketId::new("OWL-3").unwrap(),
                    description: "d OWL-3".to_owned()
                },
            ]
        );
        let sent = sent.lock().unwrap();
        let team = json!({ "key": { "eq": "OWL" } });
        assert_eq!(
            sent[0]["filter"],
            json!({ "team": team, "title": { "eqIgnoreCase": "Cap the retries" } })
        );
        assert_eq!(
            sent[1]["filter"],
            json!({ "team": team, "description": { "contains": "fingerprint 0123456789ab" } })
        );
        assert!(PROPOSED_QUERY.contains("includeArchived: true"));

        let tracker = LinearTracker::with_transport(Scripted::new(vec![]));
        assert!(tracker.proposed(None, "t", "m").is_err());
        assert!(tracker.proposed(Some("OWL"), "t", " ").is_err());
    }

    fn app_seen(app: bool, organization: &str, teams: usize) -> Value {
        json!({ "viewer": { "id": "app", "displayName": "owlshiftbot", "app": app },
            "organization": { "id": organization },
            "teams": { "nodes": vec![json!({ "id": "t1" }); teams] } })
    }

    /// With an app, comments and stage moves go through it; reading, the
    /// workflow states and mentions included, stays on the key.
    #[test]
    fn writes_go_through_the_app_and_reads_through_the_key() {
        let comment = json!({ "id": "c1", "body": "x", "createdAt": "2026-10-04T21:00:00Z",
            "editedAt": null, "user": { "id": "u2", "displayName": "owlshiftbot", "app": true },
            "botActor": null, "externalUser": null });
        let key = Scripted::new(vec![
            ("Workspace", json!({ "organization": { "id": "org-1" } })),
            (
                "Comments",
                json!({ "issue": { "comments": { "nodes": [],
                    "pageInfo": { "hasNextPage": false, "endCursor": null } } } }),
            ),
            (
                "States",
                json!({ "issue": { "id": "i1", "team": { "key": "OWL", "states": {
                    "nodes": [{ "id": "s1", "name": "Needs Input" }],
                    "pageInfo": { "hasNextPage": false } } } } }),
            ),
            (
                "Mention",
                json!({ "user": { "url": "https://linear.app/owlshift/profiles/person-1" } }),
            ),
        ]);
        let app = Scripted::new(vec![
            ("App", app_seen(true, "org-1", 1)),
            (
                "PostComment",
                json!({ "commentCreate": { "success": true, "comment": comment } }),
            ),
            (
                "SetStage",
                json!({ "issueUpdate": { "success": true,
                    "issue": { "state": { "id": "s1", "name": "Needs Input" } } } }),
            ),
        ]);
        let (by_key, by_app) = (key.asked.clone(), app.asked.clone());
        let (tracker, user) = LinearTracker::with_transport(key)
            .writing_as_app(app, "owl")
            .unwrap();
        assert_eq!(user.name, "owlshiftbot");
        // Kept from the same request, for the state gesture (OWL-270).
        assert_eq!(tracker.app_user.as_deref(), Some("app"));
        let id = TicketId::new("OWL-1").unwrap();
        tracker.comments(&id).unwrap();
        let posted = tracker.post_comment(&id, "x").unwrap();
        tracker.set_stage(&id, "Needs Input").unwrap();
        assert_eq!(
            tracker.mention("u1").unwrap().as_deref(),
            Some("https://linear.app/owlshift/profiles/person-1")
        );
        assert!(matches!(posted.author, Author::Other { .. }));
        assert_eq!(
            *by_key.lock().unwrap(),
            ["Workspace", "Comments", "States", "Mention"]
        );
        assert_eq!(*by_app.lock().unwrap(), ["App", "PostComment", "SetStage"]);
    }

    /// An app that is not an app user, belongs to another workspace than the
    /// key, or does not see the project's team is refused.
    #[test]
    fn an_app_of_another_workspace_or_blind_to_the_team_is_refused() {
        for (seen, kind, says) in [
            (
                app_seen(false, "org-1", 1),
                ErrorKind::Other,
                "not an app user",
            ),
            (
                app_seen(true, "org-2", 1),
                ErrorKind::Other,
                "another workspace",
            ),
            (
                app_seen(true, "org-1", 0),
                ErrorKind::NotFound,
                "public teams only",
            ),
        ] {
            let key = Scripted::new(vec![(
                "Workspace",
                json!({ "organization": { "id": "org-1" } }),
            )]);
            let app = Scripted::new(vec![("App", seen)]);
            let error = LinearTracker::with_transport(key)
                .writing_as_app(app, "OWL")
                .unwrap_err();
            assert_eq!(error.kind, kind, "{error}");
            assert!(error.message.contains(says), "{error}");
        }
    }

    /// A mention is a plain link to Linear. Anything else is an error that
    /// says why, so the runner can report the mention it leaves out
    /// (OWL-170).
    #[test]
    fn a_mention_is_a_plain_linear_link_or_an_error() {
        let user = |url: &str| -> &'static str {
            json!({ "data": { "user": { "url": url } } })
                .to_string()
                .leak()
        };
        let mention = |body: &'static str| tracker(vec![body]).mention("u1");
        assert_eq!(
            mention(user("https://linear.app/owlshift/profiles/person-1"))
                .unwrap()
                .as_deref(),
            Some("https://linear.app/owlshift/profiles/person-1")
        );
        for url in [
            "https://example.com/profiles/person-1",
            "https://linear.app/owlshift/profiles/a)b",
        ] {
            let error = mention(user(url)).unwrap_err();
            assert!(error.message.contains("not a plain"), "{error}");
        }
        let unknown = r#"{"errors":[{"message":"Entity not found: User",
            "extensions":{"code":"INPUT_ERROR"}}]}"#;
        let error = mention(unknown).unwrap_err();
        assert!(error.message.contains("Entity not found"), "{error}");
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

    /// A ticket's document is made with the issue's identifier and its
    /// title, then its content alone is replaced by its id, both through the
    /// app (OWL-350).
    #[test]
    fn a_document_is_made_then_updated_in_place_through_the_app() {
        let key = Scripted::new(vec![(
            "Workspace",
            json!({ "organization": { "id": "org-1" } }),
        )]);
        let written = json!({ "success": true, "document": { "id": "d1",
            "url": "https://linear.app/owlshift/document/plan-owl-1-0123abcd", "archivedAt": null } });
        let app = Scripted::new(vec![
            ("App", app_seen(true, "org-1", 1)),
            ("CreateDocument", json!({ "documentCreate": written })),
            ("UpdateDocument", json!({ "documentUpdate": written })),
        ]);
        let (by_app, sent) = (app.asked.clone(), app.sent.clone());
        let (tracker, _) = LinearTracker::with_transport(key)
            .writing_as_app(app, "owl")
            .unwrap();
        let id = TicketId::new("OWL-1").unwrap();
        let made = tracker
            .publish_document(&id, None, "Plan · OWL-1", "v1")
            .unwrap();
        let expected = Document {
            id: "d1".to_owned(),
            url: Some("https://linear.app/owlshift/document/plan-owl-1-0123abcd".to_owned()),
        };
        assert_eq!(made, expected);
        let updated = tracker
            .publish_document(&id, Some("d1"), "Plan · OWL-1", "v2")
            .unwrap();
        assert_eq!(updated, expected);
        assert_eq!(
            *by_app.lock().unwrap(),
            ["App", "CreateDocument", "UpdateDocument"]
        );
        let sent = sent.lock().unwrap();
        assert_eq!(
            sent[1],
            json!({ "input": { "issueId": "OWL-1", "title": "Plan · OWL-1", "content": "v1" } })
        );
        assert_eq!(sent[2], json!({ "id": "d1", "input": { "content": "v2" } }));
    }

    /// A document in Linear's trash, which an update still answers, and an
    /// unknown one are not found, so the runner makes another; a refusal is
    /// an error, and content past the cap is refused unsent.
    #[test]
    fn a_document_gone_is_not_found_and_a_refused_one_an_error() {
        let id = TicketId::new("OWL-1").unwrap();
        let publish = |body: &'static str| {
            tracker(vec![body])
                .publish_document(&id, Some("d1"), "Plan · OWL-1", "v2")
                .unwrap_err()
                .kind
        };
        let trashed = r#"{"data":{"documentUpdate":{"success":true,
            "document":{"id":"d1","url":null,"archivedAt":"2026-10-18T23:16:49.840Z"}}}}"#;
        assert_eq!(publish(trashed), ErrorKind::NotFound);
        let unknown = r#"{"errors":[{"message":"Entity not found: Document",
            "extensions":{"code":"INPUT_ERROR"}}]}"#;
        assert_eq!(publish(unknown), ErrorKind::NotFound);
        let refused = r#"{"data":{"documentUpdate":{"success":false,"document":null}}}"#;
        assert_eq!(publish(refused), ErrorKind::Other);
        let long = "y".repeat(MAX_DOCUMENT_CHARS + 1);
        let error = tracker(Vec::new())
            .publish_document(&id, None, "Plan · OWL-1", &long)
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Other);
    }

    /// A comment is resolved by its id alone, through the app; an unknown
    /// one is not found, and a refusal is an error (OWL-353).
    #[test]
    fn a_comment_is_resolved_through_the_app_by_its_id_alone() {
        let key = Scripted::new(vec![(
            "Workspace",
            json!({ "organization": { "id": "org-1" } }),
        )]);
        let app = Scripted::new(vec![
            ("App", app_seen(true, "org-1", 1)),
            ("Resolve", json!({ "commentResolve": { "success": true } })),
        ]);
        let (by_app, sent) = (app.asked.clone(), app.sent.clone());
        let (linear, _) = LinearTracker::with_transport(key)
            .writing_as_app(app, "owl")
            .unwrap();
        linear.resolve_comment("c1").unwrap();
        assert_eq!(*by_app.lock().unwrap(), ["App", "Resolve"]);
        assert_eq!(sent.lock().unwrap()[1], json!({ "id": "c1" }));
        let unknown = r#"{"errors":[{"message":"Entity not found: Comment",
            "extensions":{"code":"INPUT_ERROR"}}]}"#;
        let refused = r#"{"data":{"commentResolve":{"success":false}}}"#;
        let resolve = |body| tracker(vec![body]).resolve_comment("c1").unwrap_err().kind;
        assert_eq!(resolve(unknown), ErrorKind::NotFound);
        assert_eq!(resolve(refused), ErrorKind::Other);
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

    /// The rule doctor and the stage move share: exact, once. More states
    /// than one page holds is refused, not truncated; it cannot be recorded
    /// on a workspace whose team has nine.
    #[test]
    fn team_states_are_one_page_and_names_match_exactly_once() {
        let names = ["Todo", "Doing", "Doing"];
        assert_eq!(find_state(names, "Todo"), Ok(0));
        assert_eq!(find_state(names, "todo"), Err(StateMismatch::Missing));
        assert_eq!(find_state(names, "Doing"), Err(StateMismatch::Ambiguous));

        let more = r#"{"data":{"teams":{"nodes":[{"key":"OWL","states":{
            "nodes":[{"name":"Todo"}],"pageInfo":{"hasNextPage":true}}}]}}}"#;
        let error = tracker(vec![more]).team_states("OWL").unwrap_err();
        assert_eq!(error.kind, ErrorKind::Other);
        assert!(error.message.contains("more than 50"), "{error}");
    }

    /// OWL-155, checked live on 2026-10-14: the team's inbox as `propose`
    /// chooses it, the labels a team ticket can carry (the team's or the
    /// workspace's, exactly named, never another team's or a group), and
    /// the accounts Linear knows with whether each is an app or deactivated.
    #[test]
    fn admission_facts_read_the_inbox_the_labels_and_the_accounts() {
        let team = |triage: bool| {
            json!({ "data": { "teams": { "nodes": [{ "id": "team-owl", "key": "OWL",
                "triageEnabled": triage,
                "states": { "nodes": [
                    { "id": "s1", "name": "Triage", "type": "triage", "position": 0.0 },
                    { "id": "s2", "name": "Backlog", "type": "backlog", "position": 0.0 },
                    { "id": "s3", "name": "Todo", "type": "unstarted", "position": 1.0 }],
                    "pageInfo": { "hasNextPage": false } } }] } } })
            .to_string()
        };
        let labels = json!({ "data": { "issueLabels": { "nodes": [
            { "name": "agent", "isGroup": false, "team": { "id": "team-owl" } },
            { "name": "hold", "isGroup": false, "team": { "id": "team-other" } },
            { "name": "wide", "isGroup": false, "team": null },
            { "name": "group", "isGroup": true, "team": null }],
            "pageInfo": { "hasNextPage": false } } } })
        .to_string();
        let users = json!({ "data": { "users": { "nodes": [
            { "id": "u-ann", "active": true, "app": false },
            { "id": "u-bot", "active": true, "app": true },
            { "id": "u-gone", "active": false, "app": false }] } } })
        .to_string();
        let leak = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };

        let facts = tracker(vec![
            leak(team(true)),
            leak(labels.clone()),
            leak(users.clone()),
        ])
        .admission_facts(
            "owl",
            "Todo",
            &["agent", "hold", "wide", "group", "Agent"],
            &["u-ann", "u-bot", "u-gone", "u-none"],
        )
        .unwrap();
        assert_eq!(facts.inbox, Ok(false));
        assert_eq!(facts.labels, ["agent", "wide"]);
        let account = |id: &str, app, active| AccountFact {
            id: id.to_owned(),
            app,
            active,
        };
        assert_eq!(
            facts.accounts,
            [
                account("u-ann", false, true),
                account("u-bot", true, true),
                account("u-gone", false, false)
            ]
        );

        // Triage off: the backlog with the label. The ready state is refused.
        let facts = tracker(vec![leak(team(false))])
            .admission_facts("OWL", "Todo", &[], &[])
            .unwrap();
        assert_eq!(facts.inbox, Ok(true));
        assert!(facts.labels.is_empty() && facts.accounts.is_empty());
        let facts = tracker(vec![leak(team(false))])
            .admission_facts("OWL", "Backlog", &[], &[])
            .unwrap();
        assert!(facts.inbox.unwrap_err().contains("the ready state"));

        let none = r#"{"data":{"teams":{"nodes":[]}}}"#;
        let error = tracker(vec![none])
            .admission_facts("OWL", "Todo", &[], &[])
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotFound);

        let more = r#"{"data":{"issueLabels":{"nodes":[],"pageInfo":{"hasNextPage":true}}}}"#;
        let error = tracker(vec![leak(team(true)), more])
            .admission_facts("OWL", "Todo", &["agent"], &[])
            .unwrap_err();
        assert!(error.message.contains("more than 50"), "{error}");
        let many: Vec<String> = (0..51).map(|n| format!("u{n}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let error = tracker(vec![leak(team(true))])
            .admission_facts("OWL", "Todo", &[], &many)
            .unwrap_err();
        assert!(error.message.contains("51 admitters"), "{error}");
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
                app: false,
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

    fn gesture_page(creator: Value, entries: Vec<Value>, more: Option<&str>) -> &'static str {
        json!({ "data": { "issue": { "createdAt": "2026-10-01T00:00:00Z", "creator": creator,
            "botActor": null, "externalUserCreator": null,
            "history": { "nodes": entries,
                "pageInfo": { "hasNextPage": more.is_some(), "endCursor": more } } } } })
        .to_string()
        .leak()
    }

    fn actor(id: &str, app: bool) -> Value {
        json!({ "id": id, "displayName": id, "app": app })
    }

    fn added(at: &str, actor: Value, label: &str) -> Value {
        json!({ "createdAt": at, "actor": actor, "botActor": null,
            "addedLabels": [{ "name": label }], "toState": null })
    }

    /// The gesture's account (OWL-246): the latest entry that made it, read
    /// page by page until one does; an app user's entry admits nobody, nor
    /// do two entries of one time by different accounts; no entry, the
    /// creator; an answer that does not say whether an account is an app is
    /// refused.
    #[test]
    fn the_gesture_is_the_latest_entry_that_made_it_or_the_creator() {
        let id = TicketId::new("OWL-1").unwrap();
        let label = Admit::Label("agent".to_owned());
        let person = |id: &str| {
            Author::Account(Person {
                id: id.to_owned(),
                name: id.to_owned(),
            })
        };
        let other = json!({ "createdAt": "2026-10-03T00:00:00Z", "actor": actor("u9", false),
            "botActor": null, "addedLabels": [{ "name": "Agent" }], "toState": null });
        let pages = vec![
            gesture_page(actor("u0", false), vec![other], Some("c1")),
            gesture_page(
                actor("u0", false),
                vec![
                    added("2026-10-02T00:00:00Z", actor("u2", false), "agent"),
                    added("2026-10-02T00:00:00Z", actor("u2", false), "agent"),
                    added("2026-10-01T12:00:00Z", actor("u1", false), "agent"),
                ],
                Some("c2"),
            ),
        ];
        // The second page holds the gesture: no third page is read.
        let gesture = tracker(pages).with_page(3).gesture(&id, &label).unwrap();
        assert_eq!(gesture.account, person("u2"));
        assert_eq!(gesture.at, Some("2026-10-02T00:00:00Z".parse().unwrap()));

        let app = vec![added("2026-10-02T00:00:00Z", actor("u2", true), "agent")];
        let gesture = tracker(vec![gesture_page(actor("u0", false), app, None)])
            .gesture(&id, &label)
            .unwrap();
        assert!(
            matches!(gesture.account, Author::Other { .. }),
            "{gesture:?}"
        );

        let tied = vec![
            added("2026-10-02T00:00:00Z", actor("u2", false), "agent"),
            added("2026-10-02T00:00:00Z", actor("u3", false), "agent"),
        ];
        let gesture = tracker(vec![gesture_page(actor("u0", false), tied, None)])
            .gesture(&id, &label)
            .unwrap();
        assert_eq!(gesture.account, Author::unknown());

        let gesture = tracker(vec![gesture_page(actor("u0", false), vec![], None)])
            .gesture(&id, &label)
            .unwrap();
        assert_eq!(gesture.account, person("u0"));
        assert_eq!(gesture.at, Some("2026-10-01T00:00:00Z".parse().unwrap()));

        let unsaid = json!({ "id": "u0", "displayName": "u0" });
        assert!(
            tracker(vec![gesture_page(unsaid, vec![], None)])
                .gesture(&id, &label)
                .is_err()
        );
        assert!(tracker(vec![]).gesture(&id, &Admit::Delegation).is_err());
    }

    fn moved(at: &str, actor: Value, from: Option<&str>, to: &str) -> Value {
        json!({ "createdAt": at, "actor": actor, "botActor": null, "addedLabels": null,
            "fromState": from.map(|name| json!({ "name": name })), "toState": { "name": to } })
    }

    /// A tracker writing as the app user `app`, as `writing_as_app` sets.
    fn ours(bodies: Vec<&'static str>) -> LinearTracker {
        LinearTracker {
            app_user: Some("app".to_owned()),
            ..tracker(bodies)
        }
    }

    /// A state gesture (OWL-263): Owlshift's own app's moves are skipped,
    /// so a person's move to ready outlives the runner's move back; the
    /// latest other move decides, and a move out of the column by a person,
    /// a bot or another app is nobody's gesture, while another app's move to
    /// ready is that app's, which never admits (OWL-270); with only
    /// Owlshift's moves, the creator when the issue was created in ready.
    /// Without an app, no move is skipped. Read newest first, as Linear
    /// lists.
    #[test]
    fn the_state_gesture_skips_only_owlshifts_own_app_moves() {
        let at = |t: &str| format!("2026-10-0{t}T00:00:00Z");
        let app = || actor("app", true);
        let other = || actor("other", true);
        let person = |id: &str| actor(id, false);
        let by_other = Author::Other {
            name: "other".to_owned(),
        };
        let gitbot = json!({ "createdAt": at("5"), "actor": null,
            "botActor": { "name": "GitHub" }, "addedLabels": null,
            "fromState": { "name": "Todo" }, "toState": { "name": "In Review" } });
        let round = |from: &str| {
            vec![
                moved(&at("7"), app(), Some("Needs Input"), "Todo"),
                moved(&at("6"), app(), Some(from), "Needs Input"),
            ]
        };
        let cases: Vec<(&str, Vec<Value>, Author)> = vec![
            (
                "a person's move outlives the round",
                [
                    round("Todo"),
                    vec![moved(&at("4"), person("p"), Some("Backlog"), "Todo")],
                ]
                .concat(),
                Author::Account(Person {
                    id: "p".to_owned(),
                    name: "p".to_owned(),
                }),
            ),
            (
                "a person took it out of the column",
                [
                    round("Backlog"),
                    vec![
                        moved(&at("5"), person("q"), Some("Todo"), "Backlog"),
                        moved(&at("4"), person("p"), Some("Backlog"), "Todo"),
                    ],
                ]
                .concat(),
                Author::unknown(),
            ),
            (
                "a bot took it out of the column",
                [
                    round("In Review"),
                    vec![
                        gitbot,
                        moved(&at("4"), person("p"), Some("Backlog"), "Todo"),
                    ],
                ]
                .concat(),
                Author::unknown(),
            ),
            (
                "two moves of one time, one out of the column",
                vec![
                    moved(&at("4"), person("p"), Some("Backlog"), "Todo"),
                    moved(&at("4"), person("p"), Some("Todo"), "Backlog"),
                ],
                Author::unknown(),
            ),
            (
                "only the app's moves, created in ready",
                round("Todo"),
                Author::Account(Person {
                    id: "u0".to_owned(),
                    name: "u0".to_owned(),
                }),
            ),
            (
                "only the app's moves, created elsewhere",
                round("Backlog"),
                Author::unknown(),
            ),
            (
                "only the app's moves, the first not saying where from",
                vec![moved(&at("6"), app(), None, "Todo")],
                Author::unknown(),
            ),
            (
                "another app took it out of the column and back",
                vec![
                    moved(&at("6"), other(), Some("In Progress"), "Todo"),
                    moved(&at("5"), other(), Some("Todo"), "In Progress"),
                    moved(&at("4"), person("p"), Some("Backlog"), "Todo"),
                ],
                by_other.clone(),
            ),
            (
                "another app took it out, the runner's round after",
                [
                    round("In Progress"),
                    vec![
                        moved(&at("5"), other(), Some("Todo"), "In Progress"),
                        moved(&at("4"), person("p"), Some("Backlog"), "Todo"),
                    ],
                ]
                .concat(),
                Author::unknown(),
            ),
            (
                "created in ready, another app took it out and back",
                vec![
                    moved(&at("6"), other(), Some("Backlog"), "Todo"),
                    moved(&at("5"), other(), Some("Todo"), "Backlog"),
                ],
                by_other,
            ),
        ];
        let id = TicketId::new("OWL-1").unwrap();
        let todo = Admit::State("Todo".to_owned());
        for (case, entries, expected) in cases {
            let gesture = ours(vec![gesture_page(person("u0"), entries, None)])
                .gesture(&id, &todo)
                .unwrap();
            assert_eq!(gesture.account, expected, "{case}");
        }

        // Without an app, Owlshift writes through the key and no move is
        // skipped: an app's move back to ready is that app's.
        let entries = [
            round("Todo"),
            vec![moved(&at("4"), person("p"), Some("Backlog"), "Todo")],
        ]
        .concat();
        let gesture = tracker(vec![gesture_page(person("u0"), entries, None)])
            .gesture(&id, &todo)
            .unwrap();
        let by_app = Author::Other {
            name: "app".to_owned(),
        };
        assert_eq!(gesture.account, by_app);

        // Another app's move decides: the page holding it is the last read.
        let page = gesture_page(
            person("u0"),
            vec![moved(&at("5"), other(), Some("Todo"), "Backlog")],
            Some("c1"),
        );
        let gesture = ours(vec![page]).gesture(&id, &todo).unwrap();
        assert_eq!(gesture.account, Author::unknown());

        // The page of the app's moves does not decide: the next is read, and
        // the one after it is not.
        let pages = vec![
            gesture_page(person("u0"), round("Todo"), Some("c1")),
            gesture_page(
                person("u0"),
                vec![moved(&at("4"), person("p"), Some("Backlog"), "Todo")],
                Some("c2"),
            ),
        ];
        let gesture = ours(pages).with_page(2).gesture(&id, &todo).unwrap();
        assert_eq!(gesture.at, Some(at("4").parse().unwrap()));
    }

    fn admitted_page(relations: Vec<Value>, more: bool) -> &'static str {
        let issue = json!({ "identifier": "OWL-1", "title": "t", "description": null,
            "priority": 0, "assignee": null, "creator": null, "botActor": null,
            "externalUserCreator": null,
            "labels": { "nodes": [], "pageInfo": { "hasNextPage": false } },
            "createdAt": "2026-10-01T00:00:00Z",
            "inverseRelations": { "nodes": relations, "pageInfo": { "hasNextPage": more } } });
        json!({ "data": { "issues": { "nodes": [issue],
            "pageInfo": { "hasNextPage": false, "endCursor": null } } } })
        .to_string()
        .leak()
    }

    /// The listing keeps each blocker once and never drops or guesses one:
    /// a relation without its issue, a state type it does not know, and
    /// more relations than a page are refused. It needs the team.
    #[test]
    fn the_listing_refuses_what_could_release_a_ticket() {
        let label = Admit::Label("agent".to_owned());
        let column = |team| Column {
            team,
            ready: "Todo",
            admit: &label,
        };
        let blocks = |id: &str, kind: &str| json!({ "type": "blocks", "issue": { "identifier": id, "state": { "type": kind } } });
        let related = json!({ "type": "related",
            "issue": { "identifier": "OWL-3", "state": { "type": "started" } } });
        let page = admitted_page(
            vec![
                blocks("OWL-2", "canceled"),
                related,
                blocks("OWL-2", "canceled"),
            ],
            false,
        );
        let listed = tracker(vec![page]).admitted(&column(Some("owl"))).unwrap();
        assert_eq!(
            listed[0].blockers,
            [Blocker {
                id: TicketId::new("OWL-2").unwrap(),
                state: Some(StateType::Canceled),
            }]
        );

        for page in [
            admitted_page(vec![json!({ "type": "blocks", "issue": null })], false),
            admitted_page(vec![blocks("OWL-2", "paused")], false),
            admitted_page(vec![], true),
        ] {
            assert!(tracker(vec![page]).admitted(&column(Some("OWL"))).is_err());
        }
        assert!(tracker(vec![]).admitted(&column(None)).is_err());
    }

    /// The working state's tickets, every page, with their labels; the team
    /// is required (OWL-410).
    #[test]
    fn the_working_state_lists_its_tickets_with_their_labels() {
        let page = |id: &str, label: &str, next: Option<&str>| -> &'static str {
            json!({ "data": { "issues": { "nodes": [{ "identifier": id, "title": "t",
                "description": null, "priority": 0, "assignee": null, "creator": null,
                "botActor": null, "externalUserCreator": null,
                "labels": { "nodes": [{ "name": label }], "pageInfo": { "hasNextPage": false } } }],
                "pageInfo": { "hasNextPage": next.is_some(), "endCursor": next } } } })
            .to_string()
            .leak()
        };
        let working = tracker(vec![
            page("OWL-7", "zone:app", Some("c1")),
            page("OWL-8", "bug", None),
        ])
        .working(Some("owl"), "In Progress")
        .unwrap();
        let read: Vec<(&str, &[String])> = working
            .iter()
            .map(|t| (t.id.as_str(), t.labels.as_slice()))
            .collect();
        assert_eq!(
            read,
            [
                ("OWL-7", &["zone:app".to_owned()][..]),
                ("OWL-8", &["bug".to_owned()][..])
            ]
        );
        assert!(tracker(vec![]).working(None, "In Progress").is_err());
    }

    #[test]
    fn the_key_never_shows_in_debug_output() {
        let key = ApiKey::new("lin_api_do_not_print");
        let transport = HttpTransport::new(key.clone());
        let tracker = LinearTracker::new(key.clone());
        let shown = format!("{key:?} {transport:?} {tracker:?}");
        assert!(!shown.contains("do_not_print"), "{shown}");
    }

    /// The link of OWL-140 as Linear gave it on 2026-10-03; an issue Linear
    /// does not know gives none, never an error.
    #[test]
    fn a_ticket_link_is_read_and_never_fails_the_caller() {
        let id = TicketId::new("OWL-140").unwrap();
        let owl_140 = r#"{"data":{"issue":{"url":"https://linear.app/owlshift/issue/OWL-140/notify-the-operator-on-the-desktop-when-they-become-the-blocker"}}}"#;
        assert_eq!(
            tracker(vec![owl_140]).ticket_url(&id).as_deref(),
            Some(
                "https://linear.app/owlshift/issue/OWL-140/\
                 notify-the-operator-on-the-desktop-when-they-become-the-blocker"
            )
        );
        let not_found = r#"{"errors":[{"message":"Entity not found: Issue",
            "extensions":{"code":"INPUT_ERROR"}}]}"#;
        assert_eq!(tracker(vec![not_found]).ticket_url(&id), None);
    }

    /// A blocker of the text convention (OWL-250): its state type, or a
    /// definite not-found as on 2026-10-14 (OWL-264), told from another
    /// failure.
    #[test]
    fn a_state_type_is_read_and_an_unknown_issue_is_not_found() {
        let id = TicketId::new("OWL-264").unwrap();
        let started = r#"{"data":{"issue":{"state":{"type":"started"}}}}"#;
        let canceled = r#"{"data":{"issue":{"state":{"type":"canceled"}}}}"#;
        let not_found = r#"{"data":null,"errors":[{"message":"Entity not found: Issue",
            "extensions":{"code":"INPUT_ERROR","type":"invalid input"}}]}"#;
        let refused = r#"{"errors":[{"message":"boom","extensions":{"code":"INTERNAL"}}]}"#;
        let linear = tracker(vec![started, canceled, not_found, refused]);
        assert_eq!(linear.state_type(&id), Ok(Some(StateType::Started)));
        assert_eq!(linear.state_type(&id), Ok(Some(StateType::Canceled)));
        assert_eq!(
            linear.state_type(&id).unwrap_err().kind,
            ErrorKind::NotFound
        );
        assert_eq!(linear.state_type(&id).unwrap_err().kind, ErrorKind::Other);
    }

    /// The dependency check's read (OWL-249): the identifier Linear answers,
    /// whatever spelling asked, the state's type, the description, and the
    /// `blocks` blockers once each, as the listing reads them; an unknown
    /// issue is not found, as `issue(id:)` answered on 2026-10-14.
    #[test]
    fn a_tickets_relations_are_read_as_the_listing_reads_them() {
        let blocks = |id: &str, kind: &str| json!({ "type": kind, "issue": { "identifier": id, "state": { "type": "started" } } });
        let key = Scripted::new(vec![(
            "Relations",
            json!({ "issue": { "identifier": "OWL-7", "description": "Blocked by OWL-3.",
                "state": { "type": "canceled" },
                "inverseRelations": { "nodes": [blocks("OWL-2", "blocks"),
                    blocks("OWL-2", "blocks"), blocks("OWL-5", "related")],
                    "pageInfo": { "hasNextPage": false } } } }),
        )]);
        let sent = key.sent.clone();
        let asked = TicketId::new("0f3c2a52-6d2b-4e0e-9d55-6c1b1c7e2a10").unwrap();
        let relations = LinearTracker::with_transport(key)
            .relations(&asked)
            .unwrap();
        assert_eq!(sent.lock().unwrap()[0]["id"], json!(asked.as_str()));
        assert_eq!(
            relations,
            Relations {
                id: TicketId::new("OWL-7").unwrap(),
                state: Some(StateType::Canceled),
                description: "Blocked by OWL-3.".to_owned(),
                blockers: vec![Blocker {
                    id: TicketId::new("OWL-2").unwrap(),
                    state: Some(StateType::Started),
                }],
            }
        );

        let not_found = r#"{"data":null,"errors":[{"message":"Entity not found: Issue",
            "extensions":{"code":"INPUT_ERROR"}}]}"#;
        let error = tracker(vec![not_found])
            .relations(&TicketId::new("OWL-99999").unwrap())
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotFound, "{error}");
    }

    /// The relation write (OWL-249, check (d)): `blocks`, the blocker as
    /// `issueId`, by identifiers, through the app; a write Linear answers
    /// unsuccessful, or with check (d)'s `issue not found`, is an error.
    #[test]
    fn a_blocks_relation_is_written_through_the_app_by_identifiers() {
        let key = Scripted::new(vec![(
            "Workspace",
            json!({ "organization": { "id": "org-1" } }),
        )]);
        let app = Scripted::new(vec![
            ("App", app_seen(true, "org-1", 1)),
            ("Relate", related(true)),
            ("Relate", related(false)),
        ]);
        let (by_app, sent) = (app.asked.clone(), app.sent.clone());
        let (linear, _) = LinearTracker::with_transport(key)
            .writing_as_app(app, "OWL")
            .unwrap();
        let blocker = TicketId::new("OWL-271").unwrap();
        let blocked = TicketId::new("OWL-272").unwrap();
        linear.block(&blocker, &blocked).unwrap();
        assert_eq!(*by_app.lock().unwrap(), ["App", "Relate"]);
        assert_eq!(
            sent.lock().unwrap()[1]["input"],
            json!({ "type": "blocks", "issueId": "OWL-271", "relatedIssueId": "OWL-272" })
        );
        assert!(linear.block(&blocker, &blocked).is_err());

        let unknown = r#"{"data":null,"errors":[{"message":"issue not found",
            "extensions":{"type":"invalid input","code":"INPUT_ERROR","statusCode":400,
            "userError":true,"userPresentableMessage":"Issue 'OWL-99999' not found."}}]}"#;
        let error = tracker(vec![unknown])
            .block(&TicketId::new("OWL-99999").unwrap(), &blocked)
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotFound, "{error}");
    }
}
