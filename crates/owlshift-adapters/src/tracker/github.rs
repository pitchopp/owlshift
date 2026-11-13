//! The GitHub Issues tracker (OWL-535): the issues of the project's own
//! repository, read and written through its REST API with the project's
//! forge credential, the one its [`GitHubForge`] holds, so the GitHub App's
//! token is refreshed as the forge's is and no second secret exists.
//!
//! The rules are build plan "P9 · Open-source readiness", "The GitHub Issues
//! adapter", as live checks G1 to G6 confirmed them on 2026-11-12:
//!
//! - A ticket's id is the issue's number. A number that names a pull request
//!   names no ticket: its answer carries a `pull_request` key (G5), and the
//!   adapter reads it as not found. An issue moved to another repository is
//!   not found either: its answer's `url` is not this repository's.
//! - The decider is the first assignee, an account named by its numeric id,
//!   which a rename does not change. A ticket's author is never an account:
//!   whoever holds the person's token, Owlshift included, opens issues as
//!   that person, so a description is never the decider's instructions, as
//!   on Linear (OWL-74).
//! - Comments are ordered as GitHub lists them, ascending by creation and
//!   then id (G2). Their times are whole seconds, so the n-th comment of a
//!   second is read n − 1 microseconds after it, as the Markdown tracker's
//!   (OWL-136). REST has no last-edit time but `updated_at` (G2): a comment
//!   reads as edited when it differs from `created_at`. A reaction does not
//!   move it: checked live on 2026-11-12 on the sandbox's comment
//!   6827737502, `updated_at` 15:23:14 and its thumbs-up 15:23:15.
//! - The visible stage is one label per stage, the values of
//!   `[tracker.states]`; labels are matched without regard to case, as
//!   GitHub names them. A move adds the stage's label first, then removes
//!   the others, so a move cut short leaves two labels, which [`stage`]
//!   refuses to read, never none.
//! - The priority is a label named under `[tracker.priorities]`; without it
//!   every ticket reads `unset`.
//!
//! - Every read is conditional (OWL-536, G6): the forge keeps each answer's
//!   `ETag` with its body, and a 304 reuses the body without counting
//!   against the hourly limit.
//! - A comment's reactions are read when its answer counts any (G1).
//!
//! What the drain needs, admission, blockers, proposals and the working
//! state, is in `drain` (OWL-536).
//!
//! The tracker's errors have no rate-limit kind: a spent budget is
//! [`ErrorKind::Other`], its message saying so.
//!
//! [`stage`]: Tracker::stage

use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;
use serde_json::{Value, json};

use owlshift_contracts::Priority;
use owlshift_contracts::config::{self, Priorities};
use owlshift_contracts::ids::TicketId;

use owlshift_contracts::config::Admit;

use super::{
    Author, Capability, Column, Comment, Error, ErrorKind, Filed, Found, Gesture, Links, Listed,
    Person, Proposal, Reaction, Relations, StateType, Ticket, Tracker,
};
use crate::forge;
use crate::forge::github::{GitHubForge, Verb};

mod drain;
mod setup;

pub use setup::IssuesAccess;

/// The most items a page lists, GitHub's cap.
const PAGE: usize = 100;

/// The most pages of one list read: ten thousand items.
const MAX_PAGES: usize = 100;

/// The account GitHub names for one it no longer knows.
const GHOST: &str = "ghost";

/// The issues of one GitHub repository, as the project's tracker.
pub struct GitHubTracker {
    forge: GitHubForge,
    /// The stage labels, `[tracker.states]`'s values, once each.
    stages: Vec<String>,
    priorities: Option<Priorities>,
}

impl GitHubTracker {
    /// The capabilities this build implements.
    pub const CAPABILITIES: &'static [Capability] = &[
        Capability::ReadTicket,
        Capability::ListAdmitted,
        Capability::Comments,
        Capability::VisibleStage,
        Capability::BlockedBy,
        Capability::ProposalsInbox,
        Capability::Reactions,
    ];

    /// The issues of the repository `forge` reaches, through its credential,
    /// with the stage labels and priorities of the project's `tracker`.
    pub fn new(forge: GitHubForge, tracker: &config::Tracker) -> Self {
        let states = &tracker.states;
        let mut stages: Vec<String> = Vec::new();
        for state in [
            Some(&states.ready),
            Some(&states.working),
            Some(&states.needs_input),
            Some(&states.review),
            states.parked.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if !stages.iter().any(|s| s.eq_ignore_ascii_case(state)) {
                stages.push(state.clone());
            }
        }
        Self {
            forge,
            stages,
            priorities: tracker.priorities.clone(),
        }
    }

    /// `repos/<owner>/<name>/issues/<n>`.
    fn issue_path(&self, number: u64) -> String {
        let repo = self.forge.repo();
        format!("repos/{}/{}/issues/{number}", repo.owner(), repo.name())
    }

    /// The issue `id` names, refused as not found when it is a pull request
    /// or an issue of another repository.
    fn issue(&self, id: &TicketId) -> Result<(u64, Issue), Error> {
        let number = number(id)?;
        let path = self.issue_path(number);
        let issue: Issue = parse(&self.read(&path)?, id)?;
        if issue.pull_request.is_some() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("#{number} is a pull request, not an issue"),
            ));
        }
        if !self.is_this_issue(&issue.url, number) {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!(
                    "#{number} answers as {}: it moved to another repository, or the \
                     repository was renamed and `origin` names it by its old name",
                    issue.url
                ),
            ));
        }
        Ok((number, issue))
    }

    /// Whether `url` is the API URL of this repository's issue `number`,
    /// owner and name in any case.
    fn is_this_issue(&self, url: &str, number: u64) -> bool {
        let expected = format!("{}{}", forge::github::REST_API, self.issue_path(number));
        url.eq_ignore_ascii_case(&expected)
    }

    /// The stage labels `issue` carries, as GitHub names them.
    fn stage_labels<'a>(&self, issue: &'a Issue) -> Vec<&'a str> {
        issue
            .labels
            .iter()
            .map(|label| label.name.as_str())
            .filter(|name| self.stages.iter().any(|s| s.eq_ignore_ascii_case(name)))
            .collect()
    }

    /// The most urgent priority whose label `labels` holds.
    fn priority(&self, labels: &[String]) -> Priority {
        let Some(priorities) = &self.priorities else {
            return Priority::Unset;
        };
        priorities
            .labels()
            .into_iter()
            .find(|(_, label)| labels.iter().any(|l| l.eq_ignore_ascii_case(label)))
            .map_or(Priority::Unset, |(priority, _)| priority)
    }

    /// `repos/<owner>/<name>`, which every path of the tracker starts with.
    fn repo_path(&self) -> String {
        let repo = self.forge.repo();
        format!("repos/{}/{}", repo.owner(), repo.name())
    }

    /// One read, conditional on the answer kept from the last
    /// ([`GitHubForge::rest_text_cached`], OWL-536).
    fn read(&self, path: &str) -> Result<String, Error> {
        self.forge.rest_text_cached(path).map_err(tracker_error)
    }

    /// Every item of the list at `path`, [`PAGE`] a page to the first
    /// short page, `what` naming it in an error. More than [`MAX_PAGES`]
    /// pages is an error, never a list cut short.
    fn every<T: for<'de> Deserialize<'de>>(&self, path: &str, what: &str) -> Result<Vec<T>, Error> {
        let sep = if path.contains('?') { '&' } else { '?' };
        let mut read: Vec<T> = Vec::new();
        for page in 1..=MAX_PAGES {
            let text = self.read(&format!("{path}{sep}per_page={PAGE}&page={page}"))?;
            let more: Vec<T> = parse(&text, &what)?;
            let last = more.len() < PAGE;
            read.extend(more);
            if last {
                return Ok(read);
            }
        }
        Err(Error::new(
            ErrorKind::Other,
            format!("{what} run past {MAX_PAGES} pages"),
        ))
    }

    /// Every comment of issue `number`, every page read, with the reactions
    /// of each that has any (OWL-536): a comment's answer counts them, so a
    /// comment without one costs no read.
    fn every_comment(&self, number: u64, id: &TicketId) -> Result<Vec<Comment>, Error> {
        let path = format!("{}/comments", self.issue_path(number));
        let read: Vec<RestComment> = self.every(&path, &format!("the comments of {id}"))?;
        let reacted: Vec<String> = read
            .iter()
            .filter(|c| c.reactions.as_ref().is_some_and(|r| r.total_count > 0))
            .map(|c| c.id.to_string())
            .collect();
        let mut comments = in_order(read, |url| self.is_this_issue(url, number))?;
        for comment in &mut comments {
            if reacted.contains(&comment.id) {
                comment.reactions = self.reactions(&comment.id)?;
            }
        }
        Ok(comments)
    }

    /// The reactions on comment `comment`, in GitHub's order, each by the
    /// rule of a comment's author (G1).
    fn reactions(&self, comment: &str) -> Result<Vec<Reaction>, Error> {
        let path = format!("{}/issues/comments/{comment}/reactions", self.repo_path());
        let read: Vec<RestReaction> =
            self.every(&path, &format!("the reactions of comment {comment}"))?;
        read.into_iter()
            .map(|reaction| {
                Ok(Reaction {
                    emoji: reaction.content,
                    author: reaction.user.map_or_else(Author::unknown, User::author),
                    created_at: timestamp(&reaction.created_at)?,
                })
            })
            .collect()
    }

    /// The ticket an issue's answer reads as, under `id`.
    fn to_ticket(&self, id: TicketId, issue: Issue) -> Ticket {
        let labels: Vec<String> = issue.labels.into_iter().map(|l| l.name).collect();
        Ticket {
            id,
            title: issue.title,
            description: issue.body.unwrap_or_default(),
            priority: self.priority(&labels),
            assignee: issue
                .assignees
                .into_iter()
                .find_map(|user| match user.author() {
                    Author::Account(person) => Some(person),
                    Author::Other { .. } => None,
                }),
            labels,
            author: match issue.user.map(User::author) {
                Some(Author::Account(person)) => Author::Other { name: person.name },
                Some(other) => other,
                None => Author::unknown(),
            },
        }
    }
}

impl Tracker for GitHubTracker {
    fn capabilities(&self) -> &'static [Capability] {
        Self::CAPABILITIES
    }

    fn ticket(&self, id: &TicketId) -> Result<Ticket, Error> {
        let (_, issue) = self.issue(id)?;
        Ok(self.to_ticket(id.clone(), issue))
    }

    fn comments(&self, id: &TicketId) -> Result<Vec<Comment>, Error> {
        let (number, _) = self.issue(id)?;
        self.every_comment(number, id)
    }

    /// Posts it, then reads the comments again, so that the comment
    /// returned has the time the list gives it among the comments of its
    /// second.
    fn post_comment(&self, id: &TicketId, body: &str) -> Result<Comment, Error> {
        let (number, _) = self.issue(id)?;
        let path = format!("{}/comments", self.issue_path(number));
        let text = self
            .forge
            .rest_write(Verb::Post, &path, &json!({ "body": body }))
            .map_err(tracker_error)?;
        let posted: RestComment = parse(&text, id)?;
        let posted_id = posted.id;
        let listed = self.every_comment(number, id)?;
        match listed.into_iter().find(|c| c.id == posted_id.to_string()) {
            Some(comment) => Ok(comment),
            // The list may lag behind the write: the comment as answered.
            None => posted.into_comment(SignedDuration::ZERO),
        }
    }

    fn set_stage(&self, id: &TicketId, state: &str) -> Result<(), Error> {
        let Some(wanted) = self.stages.iter().find(|s| s.eq_ignore_ascii_case(state)) else {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "{state:?} is not one of the project's stage labels ({}): refused",
                    self.stages.join(", ")
                ),
            ));
        };
        let (number, issue) = self.issue(id)?;
        let carried = self.stage_labels(&issue);
        if !carried.iter().any(|l| l.eq_ignore_ascii_case(wanted)) {
            let path = format!("{}/labels", self.issue_path(number));
            self.forge
                .rest_write(Verb::Post, &path, &json!({ "labels": [wanted] }))
                .map_err(tracker_error)?;
        }
        let others: Vec<&str> = carried
            .into_iter()
            .filter(|l| !l.eq_ignore_ascii_case(wanted))
            .collect();
        for (i, other) in others.iter().enumerate() {
            let path = format!("{}/labels/{}", self.issue_path(number), encode(other));
            match self.forge.rest_write(Verb::Delete, &path, &Value::Null) {
                Ok(_) => {}
                // Removed meanwhile: what the move wanted.
                Err(e) if e.kind == forge::ErrorKind::NotFound => {}
                Err(e) => {
                    let error = tracker_error(e);
                    return Err(Error::new(
                        error.kind,
                        format!(
                            "{id} carries {wanted:?} and still {}: {error}",
                            others[i..].join(", ")
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// `closed` for a closed issue, whatever its labels; for an open one its
    /// stage label, or `open` when it carries none. Several stage labels are
    /// an error: the stage cannot be told.
    fn stage(&self, id: &TicketId) -> Result<String, Error> {
        let (_, issue) = self.issue(id)?;
        if issue.state == "closed" {
            return Ok("closed".to_owned());
        }
        match self.stage_labels(&issue)[..] {
            [] => Ok("open".to_owned()),
            [one] => Ok(self
                .stages
                .iter()
                .find(|s| s.eq_ignore_ascii_case(one))
                .cloned()
                .unwrap_or_else(|| one.to_owned())),
            ref several => Err(Error::new(
                ErrorKind::Other,
                format!(
                    "{id} carries several stage labels ({}): remove all but one",
                    several.join(", ")
                ),
            )),
        }
    }

    fn ticket_url(&self, id: &TicketId) -> Option<String> {
        let number = number(id).ok()?;
        let repo = self.forge.repo();
        Some(format!(
            "https://github.com/{}/{}/issues/{number}",
            repo.owner(),
            repo.name()
        ))
    }

    /// `#12`, which the text convention reads; anything that is no issue
    /// number as it is.
    fn reference(&self, id: &TicketId) -> String {
        match number(id) {
            Ok(n) => format!("#{n}"),
            Err(_) => id.as_str().to_owned(),
        }
    }

    /// `Closes #12`: GitHub links the pull request to the issue, and closes
    /// it as completed when the pull request merges (G5).
    fn closing_line(&self, id: &TicketId) -> Option<String> {
        number(id).ok().map(|n| format!("Closes #{n}"))
    }

    fn admitted(&self, column: &Column<'_>) -> Result<Vec<Listed>, Error> {
        self.listed(column)
    }

    fn working(&self, _team: Option<&str>, state: &str) -> Result<Vec<Ticket>, Error> {
        self.labelled(state)
    }

    fn gesture(&self, id: &TicketId, admit: &Admit) -> Result<Gesture, Error> {
        self.label_gesture(id, admit)
    }

    fn state_type(&self, id: &TicketId) -> Result<Option<StateType>, Error> {
        let (_, issue) = self.issue(id)?;
        Ok(drain::state_type(&issue))
    }

    fn relations(&self, id: &TicketId) -> Result<Relations, Error> {
        self.issue_relations(id)
    }

    fn links(&self, id: &TicketId) -> Result<Links, Error> {
        self.issue_links(id)
    }

    fn block(&self, blocker: &TicketId, blocked: &TicketId) -> Result<(), Error> {
        self.write_block(blocker, blocked)
    }

    fn propose(&self, proposal: &Proposal<'_>) -> Result<Filed, Error> {
        self.open_proposal(proposal)
    }

    fn proposed(
        &self,
        _team: Option<&str>,
        title: &str,
        marker: &str,
    ) -> Result<Vec<Found>, Error> {
        self.find_proposals(title, marker)
    }
}

/// The issue number `id` names: digits only, without a leading zero.
/// Anything else names no issue of GitHub's, and is not found without a
/// request.
fn number(id: &TicketId) -> Result<u64, Error> {
    let text = id.as_str();
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    match text.parse::<u64>() {
        Ok(n) if digits && n > 0 && !text.starts_with('0') => Ok(n),
        _ => Err(Error::new(
            ErrorKind::NotFound,
            format!("{id} is not an issue number: a GitHub ticket is named by its number, `12`"),
        )),
    }
}

/// The comments `read` in the list's order, each of one second read a
/// microsecond after the one before it in that second; refused when one
/// is not a comment of the issue `ours` accepts the URL of.
fn in_order(read: Vec<RestComment>, ours: impl Fn(&str) -> bool) -> Result<Vec<Comment>, Error> {
    let mut comments: Vec<Comment> = Vec::with_capacity(read.len());
    let mut previous: Option<(Timestamp, i64)> = None;
    for comment in read {
        if !ours(&comment.issue_url) {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "GitHub listed comment {} of {}, another issue",
                    comment.id, comment.issue_url
                ),
            ));
        }
        let second = timestamp(&comment.created_at)?;
        let nth = match previous {
            Some((last, n)) if last == second => n + 1,
            _ => 0,
        };
        previous = Some((second, nth));
        comments.push(comment.into_comment(SignedDuration::from_micros(nth))?);
    }
    Ok(comments)
}

fn timestamp(text: &str) -> Result<Timestamp, Error> {
    text.parse().map_err(|_| {
        Error::new(
            ErrorKind::Other,
            format!("GitHub answered a time that does not read: {text:?}"),
        )
    })
}

/// Parses an answer, without repeating its words in the error.
fn parse<T: for<'de> Deserialize<'de>>(
    text: &str,
    what: &dyn std::fmt::Display,
) -> Result<T, Error> {
    serde_json::from_str(text).map_err(|e| {
        Error::new(
            ErrorKind::Other,
            format!("unexpected answer from GitHub for {what}: {e}"),
        )
    })
}

/// The tracker's error for the forge's.
fn tracker_error(error: forge::Error) -> Error {
    let kind = match error.kind {
        forge::ErrorKind::NotFound => ErrorKind::NotFound,
        forge::ErrorKind::Unauthorized | forge::ErrorKind::SignInAgain => ErrorKind::Unauthorized,
        forge::ErrorKind::Unreachable => ErrorKind::Unreachable,
        forge::ErrorKind::RateLimited | forge::ErrorKind::HeadMoved | forge::ErrorKind::Other => {
            ErrorKind::Other
        }
    };
    let message = if error.kind == forge::ErrorKind::Unauthorized {
        format!(
            "{}: the token needs Issues (read and write) on the repository, which a GitHub \
             App's installation must have accepted",
            error.message
        )
    } else {
        error.message
    };
    Error::new(kind, message)
}

/// `text` as one segment of a URL path: every byte but the unreserved ones
/// percent-encoded.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[derive(Deserialize)]
struct Issue {
    /// The issue's database id, which the dependency API names it by (G4).
    #[serde(default)]
    id: u64,
    #[serde(default)]
    number: u64,
    url: String,
    /// The API URL of its repository, which tells a blocker of another
    /// repository (G4).
    #[serde(default)]
    repository_url: String,
    title: String,
    body: Option<String>,
    user: Option<User>,
    #[serde(default)]
    assignees: Vec<User>,
    #[serde(default)]
    labels: Vec<Label>,
    state: String,
    /// `completed`, `not_planned`, `duplicate` or `reopened` (G4); null for
    /// an open issue, and for one closed before GitHub recorded a reason.
    #[serde(default)]
    state_reason: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    /// Set for a pull request (G5).
    pull_request: Option<PullLink>,
}

/// The `pull_request` key of an issue's answer.
#[derive(Deserialize)]
struct PullLink {
    #[serde(default)]
    merged_at: Option<String>,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
struct User {
    login: String,
    id: u64,
    #[serde(rename = "type")]
    kind: String,
}

impl User {
    /// A person's account only for a user GitHub still knows: a bot, an
    /// organization, a mannequin or the ghost of a deleted account is named
    /// alone.
    fn author(self) -> Author {
        if self.kind == "User" && self.login != GHOST {
            Author::Account(Person {
                id: self.id.to_string(),
                name: self.login,
            })
        } else {
            Author::Other { name: self.login }
        }
    }
}

#[derive(Deserialize)]
struct RestComment {
    id: u64,
    issue_url: String,
    user: Option<User>,
    created_at: String,
    updated_at: String,
    body: Option<String>,
    /// The counts of its reactions (G1).
    #[serde(default)]
    reactions: Option<ReactionCounts>,
}

#[derive(Deserialize)]
struct ReactionCounts {
    #[serde(default)]
    total_count: u64,
}

/// A reaction as GitHub lists it (G1).
#[derive(Deserialize)]
struct RestReaction {
    content: String,
    user: Option<User>,
    created_at: String,
}

impl RestComment {
    /// The comment, its creation read `offset` after its second.
    fn into_comment(self, offset: SignedDuration) -> Result<Comment, Error> {
        let created = timestamp(&self.created_at)?;
        let updated = timestamp(&self.updated_at)?;
        let created_at = created.checked_add(offset).map_err(|_| {
            Error::new(
                ErrorKind::Other,
                format!("comment {} has a time out of range", self.id),
            )
        })?;
        Ok(Comment {
            id: self.id.to_string(),
            author: self.user.map_or_else(Author::unknown, User::author),
            created_at,
            edited_at: (updated > created).then_some(updated),
            body: self.body.unwrap_or_default(),
            parent: None,
            reactions: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::forge::Repo;
    use crate::graphql::{Response, RestAnswer, Transport};

    pub(super) const ISSUE: &str = "repos/demo/project/issues/7";

    /// Answers each REST call from its method and path, and keeps the calls.
    /// A read answered with an `ETag` (`on_tagged`) and asked again with it
    /// answers HTTP 304, kept as `GET <path> (304)`.
    #[derive(Clone, Default)]
    pub(super) struct Canned {
        answers: Arc<Mutex<Vec<Answer>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    /// A call, its status, its body and its `ETag`.
    type Answer = (String, u16, Value, Option<String>);

    impl Canned {
        pub(super) fn on(self, call: &str, status: u16, body: Value) -> Self {
            self.answers
                .lock()
                .unwrap()
                .push((call.to_owned(), status, body, None));
            self
        }

        /// [`Self::on`], the answer carrying the `ETag` `etag`.
        pub(super) fn on_tagged(self, call: &str, body: Value, etag: &str) -> Self {
            self.answers
                .lock()
                .unwrap()
                .push((call.to_owned(), 200, body, Some(etag.to_owned())));
            self
        }

        /// The first answer given for `call`; one given again later for the
        /// same call replaces it once it has answered, so a call can answer
        /// differently on its next asking (OWL-537).
        fn answer(&self, call: String) -> (u16, String, Option<String>) {
            self.calls.lock().unwrap().push(call.clone());
            let mut answers = self.answers.lock().unwrap();
            let at = answers
                .iter()
                .position(|(c, _, _, _)| *c == call)
                .unwrap_or_else(|| panic!("no answer for {call}"));
            let (_, status, body, etag) = answers[at].clone();
            if answers[at + 1..].iter().any(|(c, _, _, _)| *c == call) {
                answers.remove(at);
            }
            (status, body.to_string(), etag)
        }

        pub(super) fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Transport for Canned {
        fn send(&self, request: &Value) -> Result<Response, String> {
            panic!("no GraphQL here: {request}")
        }

        fn get(&self, path: &str) -> Result<RestAnswer, String> {
            let (status, body, etag) = self.answer(format!("GET {path}"));
            let answer = RestAnswer::new(status, body);
            Ok(match etag {
                Some(etag) => answer.with_etag(etag),
                None => answer,
            })
        }

        fn get_if_none_match(&self, path: &str, etag: &str) -> Result<RestAnswer, String> {
            let kept = self
                .answers
                .lock()
                .unwrap()
                .iter()
                .find(|(c, _, _, _)| *c == format!("GET {path}"))
                .and_then(|(_, _, _, tag)| tag.clone());
            if kept.as_deref() == Some(etag) {
                self.calls.lock().unwrap().push(format!("GET {path} (304)"));
                return Ok(RestAnswer::new(304, "").with_etag(etag));
            }
            self.get(path)
        }

        fn post(&self, path: &str, body: &Value) -> Result<Response, String> {
            let (status, answer, _) = self.answer(format!("POST {path} {body}"));
            Ok(Response::new(status, answer))
        }

        fn delete(&self, path: &str) -> Result<Response, String> {
            let (status, answer, _) = self.answer(format!("DELETE {path}"));
            Ok(Response::new(status, answer))
        }
    }

    pub(super) fn config(priorities: Option<Priorities>) -> config::Tracker {
        config::Tracker {
            kind: config::TrackerKind::Github,
            team: None,
            admit: config::Admit::Label("agent:ready".to_owned()),
            admitters: None,
            exclude: None,
            states: config::States {
                ready: "agent:ready".to_owned(),
                working: "agent:working".to_owned(),
                needs_input: "agent:needs input".to_owned(),
                review: "agent:review".to_owned(),
                parked: None,
            },
            priorities,
        }
    }

    pub(super) fn tracker(canned: &Canned) -> GitHubTracker {
        tracker_with(canned, None)
    }

    fn tracker_with(canned: &Canned, priorities: Option<Priorities>) -> GitHubTracker {
        let forge =
            GitHubForge::with_transport(canned.clone(), Repo::parse("demo/project").unwrap());
        GitHubTracker::new(forge, &config(priorities))
    }

    pub(super) fn issue(labels: &[&str], state: &str) -> Value {
        json!({
            "url": "https://api.github.com/repos/Demo/Project/issues/7",
            "number": 7,
            "title": "Say hello",
            "body": null,
            "user": { "login": "ann", "id": 1, "type": "User" },
            "assignees": [
                { "login": "a-bot", "id": 9, "type": "Bot" },
                { "login": "bea", "id": 2, "type": "User" }
            ],
            "labels": labels.iter().map(|name| json!({ "name": name })).collect::<Vec<_>>(),
            "state": state,
        })
    }

    pub(super) fn comment(id: u64, created: &str, updated: &str, user: Value) -> Value {
        json!({
            "id": id,
            "issue_url": format!("https://api.github.com/{ISSUE}"),
            "user": user,
            "created_at": created,
            "updated_at": updated,
            "body": format!("comment {id}"),
        })
    }

    pub(super) fn id(text: &str) -> TicketId {
        TicketId::new(text).unwrap()
    }

    #[test]
    fn a_ticket_reads_its_decider_labels_and_never_its_author_as_an_account() {
        let canned = Canned::default().on(&format!("GET {ISSUE}"), 200, issue(&["Bug"], "open"));
        let ticket = tracker(&canned).ticket(&id("7")).unwrap();
        assert_eq!(ticket.title, "Say hello");
        assert_eq!(ticket.description, "");
        assert_eq!(ticket.labels, ["Bug"]);
        assert_eq!(ticket.priority, Priority::Unset);
        assert_eq!(
            ticket.assignee,
            Some(Person {
                id: "2".to_owned(),
                name: "bea".to_owned()
            })
        );
        assert_eq!(
            ticket.author,
            Author::Other {
                name: "ann".to_owned()
            }
        );
    }

    #[test]
    fn a_pull_request_an_issue_moved_away_and_a_name_are_not_found() {
        let mut pull = issue(&[], "open");
        pull["pull_request"] =
            json!({ "url": "https://api.github.com/repos/demo/project/pulls/7" });
        let canned = Canned::default().on(&format!("GET {ISSUE}"), 200, pull);
        let error = tracker(&canned).ticket(&id("7")).unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotFound, "{error}");
        assert!(error.message.contains("pull request"), "{error}");

        let mut moved = issue(&[], "open");
        moved["url"] = json!("https://api.github.com/repos/demo/elsewhere/issues/3");
        let canned = Canned::default().on(&format!("GET {ISSUE}"), 200, moved);
        let error = tracker(&canned).comments(&id("7")).unwrap_err();
        assert_eq!(error.kind, ErrorKind::NotFound, "{error}");

        let canned = Canned::default();
        for text in ["OWL-7", "07", "0"] {
            let error = tracker(&canned).ticket(&id(text)).unwrap_err();
            assert_eq!(error.kind, ErrorKind::NotFound, "{text}: {error}");
        }
        assert!(canned.calls().is_empty(), "{:?}", canned.calls());
        assert_eq!(tracker(&canned).ticket_url(&id("OWL-7")), None);
        assert_eq!(
            tracker(&canned).ticket_url(&id("7")).as_deref(),
            Some("https://github.com/demo/project/issues/7")
        );
    }

    #[test]
    fn a_refused_token_names_the_issues_permission() {
        let canned = Canned::default().on(
            &format!("GET {ISSUE}"),
            403,
            json!({ "message": "Resource not accessible by integration" }),
        );
        let error = tracker(&canned).ticket(&id("7")).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unauthorized);
        assert!(error.message.contains("Issues (read and write)"), "{error}");
    }

    #[test]
    fn the_priority_is_the_most_urgent_label_named() {
        let priorities = Priorities {
            urgent: Some("P0".to_owned()),
            high: Some("P1".to_owned()),
            medium: None,
            low: Some("P3".to_owned()),
        };
        let canned =
            Canned::default().on(&format!("GET {ISSUE}"), 200, issue(&["p3", "p1"], "open"));
        let ticket = tracker_with(&canned, Some(priorities))
            .ticket(&id("7"))
            .unwrap();
        assert_eq!(ticket.priority, Priority::High);
    }

    #[test]
    fn comments_of_one_second_read_in_the_lists_order() {
        let ann = json!({ "login": "ann", "id": 1, "type": "User" });
        let page = json!([
            comment(
                10,
                "2026-11-12T15:23:13Z",
                "2026-11-12T15:23:14Z",
                ann.clone()
            ),
            comment(
                11,
                "2026-11-12T15:23:18Z",
                "2026-11-12T15:23:18Z",
                ann.clone()
            ),
            comment(12, "2026-11-12T15:23:18Z", "2026-11-12T15:23:26Z", ann),
            comment(
                13,
                "2026-11-12T15:23:18Z",
                "2026-11-12T15:23:18Z",
                json!({ "login": "a-bot[bot]", "id": 9, "type": "Bot" })
            ),
            comment(
                14,
                "2026-11-12T15:23:19Z",
                "2026-11-12T15:23:19Z",
                json!({ "login": "ghost", "id": 10137, "type": "User" })
            ),
        ]);
        let canned = Canned::default()
            .on(&format!("GET {ISSUE}"), 200, issue(&[], "open"))
            .on(
                &format!("GET {ISSUE}/comments?per_page=100&page=1"),
                200,
                page,
            );
        let comments = tracker(&canned).comments(&id("7")).unwrap();
        let times: Vec<String> = comments.iter().map(|c| c.created_at.to_string()).collect();
        assert_eq!(
            times,
            [
                "2026-11-12T15:23:13Z",
                "2026-11-12T15:23:18Z",
                "2026-11-12T15:23:18.000001Z",
                "2026-11-12T15:23:18.000002Z",
                "2026-11-12T15:23:19Z",
            ]
        );
        let edited: Vec<Option<String>> = comments
            .iter()
            .map(|c| c.edited_at.map(|t| t.to_string()))
            .collect();
        assert_eq!(
            edited,
            [
                Some("2026-11-12T15:23:14Z".to_owned()),
                None,
                Some("2026-11-12T15:23:26Z".to_owned()),
                None,
                None,
            ]
        );
        assert!(matches!(comments[0].author, Author::Account(_)));
        assert!(matches!(comments[3].author, Author::Other { .. }));
        assert!(matches!(comments[4].author, Author::Other { .. }));
    }

    #[test]
    fn a_full_page_of_comments_reads_the_next() {
        let ann = json!({ "login": "ann", "id": 1, "type": "User" });
        let full: Vec<Value> = (1..=100)
            .map(|n| {
                comment(
                    n,
                    "2026-11-12T15:00:00Z",
                    "2026-11-12T15:00:00Z",
                    ann.clone(),
                )
            })
            .collect();
        let canned = Canned::default()
            .on(&format!("GET {ISSUE}"), 200, issue(&[], "open"))
            .on(
                &format!("GET {ISSUE}/comments?per_page=100&page=1"),
                200,
                json!(full),
            )
            .on(
                &format!("GET {ISSUE}/comments?per_page=100&page=2"),
                200,
                json!([]),
            );
        let comments = tracker(&canned).comments(&id("7")).unwrap();
        assert_eq!(comments.len(), 100);
        assert_eq!(
            comments[99].created_at.to_string(),
            "2026-11-12T15:00:00.000099Z"
        );
    }

    #[test]
    fn the_stage_is_closed_open_or_the_one_stage_label() {
        for (labels, state, read) in [
            (&["agent:working"][..], "closed", Ok("closed")),
            (&["Bug"][..], "open", Ok("open")),
            (&["Bug", "Agent:Working"][..], "open", Ok("agent:working")),
            (&["agent:working", "agent:review"][..], "open", Err(())),
        ] {
            let canned = Canned::default().on(&format!("GET {ISSUE}"), 200, issue(labels, state));
            let found = tracker(&canned).stage(&id("7"));
            match read {
                Ok(name) => assert_eq!(found.unwrap(), name, "{labels:?}"),
                Err(()) => assert!(found.is_err(), "{labels:?}: {found:?}"),
            }
        }
    }

    #[test]
    fn a_move_adds_the_stage_label_then_removes_the_others() {
        let canned = Canned::default()
            .on(
                &format!("GET {ISSUE}"),
                200,
                issue(&["Bug", "agent:working", "agent:review"], "open"),
            )
            .on(
                &format!(r#"POST {ISSUE}/labels {{"labels":["agent:needs input"]}}"#),
                200,
                json!([]),
            )
            .on(
                &format!("DELETE {ISSUE}/labels/agent%3Aworking"),
                200,
                json!([]),
            )
            .on(
                &format!("DELETE {ISSUE}/labels/agent%3Areview"),
                404,
                json!({ "message": "Label does not exist" }),
            );
        tracker(&canned)
            .set_stage(&id("7"), "Agent:Needs Input")
            .unwrap();
        assert_eq!(
            canned.calls(),
            [
                format!("GET {ISSUE}"),
                format!(r#"POST {ISSUE}/labels {{"labels":["agent:needs input"]}}"#),
                format!("DELETE {ISSUE}/labels/agent%3Aworking"),
                format!("DELETE {ISSUE}/labels/agent%3Areview"),
            ]
        );

        // Already there: nothing is added.
        let canned = Canned::default().on(
            &format!("GET {ISSUE}"),
            200,
            issue(&["agent:review"], "open"),
        );
        tracker(&canned)
            .set_stage(&id("7"), "agent:review")
            .unwrap();
        assert_eq!(canned.calls(), [format!("GET {ISSUE}")]);
    }

    #[test]
    fn a_move_refuses_a_name_that_is_no_stage_and_says_what_a_failed_removal_left() {
        let canned = Canned::default();
        let error = tracker(&canned).set_stage(&id("7"), "Done").unwrap_err();
        assert!(error.message.contains("not one of"), "{error}");
        assert!(canned.calls().is_empty());

        let canned = Canned::default()
            .on(
                &format!("GET {ISSUE}"),
                200,
                issue(&["agent:working"], "open"),
            )
            .on(
                &format!(r#"POST {ISSUE}/labels {{"labels":["agent:review"]}}"#),
                200,
                json!([]),
            )
            .on(
                &format!("DELETE {ISSUE}/labels/agent%3Aworking"),
                500,
                json!({ "message": "Server Error" }),
            );
        let error = tracker(&canned)
            .set_stage(&id("7"), "agent:review")
            .unwrap_err();
        assert!(error.message.contains("still agent:working"), "{error}");
    }
}
