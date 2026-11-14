//! What the drain reads and writes on GitHub Issues (OWL-536): the admitted
//! issues and the gesture's account, the state a blocker shows, the
//! blockers by GitHub's dependency API, proposals and the in-flight set.
//! The rules are build plan "P9 · Open-source readiness", "The GitHub
//! Issues adapter", as live checks G1, G3, G4 and G6 confirmed them.

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::json;

use owlshift_contracts::config::Admit;
use owlshift_contracts::ids::TicketId;

use super::{GitHubTracker, Issue, Label, User, encode, number, parse, timestamp, tracker_error};
use crate::forge::github::{REST_API, Verb};
use crate::tracker::{
    Author, Blocker, Column, Error, ErrorKind, Filed, Found, Gesture, Linked, LinkedState, Links,
    Listed, PROPOSED_LABEL, Proposal, Relations, StateType, Ticket, no_delegation,
};

/// The type of the state an issue's answer shows (G4). Open is no type: it
/// holds what it blocks. Closed as completed is completed, and so is a
/// closed issue with no reason, which GitHub gives an issue closed before it
/// recorded one, its close being completed by default. Closed as not
/// planned is canceled, as a duplicate a duplicate: both hold. A pull
/// request, among blockers, is completed once merged and canceled when
/// closed without a merge. An unknown reason is no type, and holds.
pub(super) fn state_type(issue: &Issue) -> Option<StateType> {
    if issue.state != "closed" {
        return None;
    }
    if let Some(pull) = &issue.pull_request {
        return Some(if pull.merged_at.is_some() {
            StateType::Completed
        } else {
            StateType::Canceled
        });
    }
    match issue.state_reason.as_deref() {
        None | Some("completed") => Some(StateType::Completed),
        Some("not_planned") => Some(StateType::Canceled),
        Some("duplicate") => Some(StateType::Duplicate),
        Some(_) => None,
    }
}

/// The last edit of an issue's description (OWL-557).
const LAST_EDITED_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) { \
     repository(owner: $owner, name: $name) { issue(number: $number) { lastEditedAt } } }";

/// An issue event (G3): the issues' events, ascending by id.
#[derive(Deserialize)]
struct Event {
    event: String,
    actor: Option<User>,
    label: Option<Label>,
    created_at: String,
    /// Set when the event was made through a GitHub App: a label put
    /// through the app's user token carries `owl-shift` (G3).
    performed_via_github_app: Option<App>,
}

#[derive(Deserialize)]
struct App {
    #[serde(default)]
    slug: Option<String>,
}

impl Event {
    fn of_label(&self, label: &str) -> bool {
        self.label
            .as_ref()
            .is_some_and(|l| l.name.eq_ignore_ascii_case(label))
    }
}

impl GitHubTracker {
    /// `repos/<owner>/<name>/issues`.
    fn issues_path(&self) -> String {
        format!("{}/issues", self.repo_path())
    }

    /// The open issues carrying every one of `labels`, pull requests left
    /// out, each once, in GitHub's order. GitHub ANDs the labels of one
    /// list (`labels=a,b`); a label holding a comma cannot be asked so.
    fn open_labelled(&self, labels: &[&str], what: &str) -> Result<Vec<Issue>, Error> {
        if let Some(label) = labels.iter().find(|l| l.contains(',')) {
            return Err(Error::new(
                ErrorKind::Other,
                format!("the label {label:?} holds a comma, which GitHub's list cannot ask for"),
            ));
        }
        let asked: Vec<String> = labels.iter().map(|l| encode(l)).collect();
        let path = format!(
            "{}?state=open&labels={}",
            self.issues_path(),
            asked.join(",")
        );
        let mut issues: Vec<Issue> = self.every(&path, what)?;
        let mut seen = Vec::new();
        issues.retain(|issue| {
            let new = issue.pull_request.is_none() && !seen.contains(&issue.number);
            seen.push(issue.number);
            new
        });
        Ok(issues)
    }

    /// The ticket of an issue a list gave.
    fn listed_ticket(&self, issue: Issue) -> Result<Ticket, Error> {
        let id = issue_id(issue.number)?;
        Ok(self.to_ticket(id, issue))
    }

    /// The open issues carrying the ready label, and the admission label
    /// when it is another: the ready column, as Linear's is the ready state
    /// with the label. Each with its blockers; a blocker list that cannot be
    /// read fails the whole listing, never reads as no blocker.
    pub(super) fn listed(&self, column: &Column<'_>) -> Result<Vec<Listed>, Error> {
        let mut labels = vec![column.ready];
        match column.admit {
            Admit::Label(label) if !label.eq_ignore_ascii_case(column.ready) => labels.push(label),
            Admit::Label(_) | Admit::State(_) => {}
            Admit::Delegation => return Err(no_delegation()),
        }
        let issues = self.open_labelled(&labels, "the ready column")?;
        let mut listed = Vec::with_capacity(issues.len());
        for issue in issues {
            let created_at = issue.created_at.as_deref().map(timestamp).transpose()?;
            let blockers = self.blockers(issue.number)?;
            listed.push(Listed {
                ticket: self.listed_ticket(issue)?,
                created_at,
                blockers,
            });
        }
        Ok(listed)
    }

    /// The open issues carrying the label `state`: the working state's
    /// tickets (OWL-410), pull requests left out.
    pub(super) fn labelled(&self, state: &str) -> Result<Vec<Ticket>, Error> {
        self.open_labelled(&[state], "the working state")?
            .into_iter()
            .map(|issue| self.listed_ticket(issue))
            .collect()
    }

    /// The last `labeled` event of the gesture's label that no `unlabeled`
    /// event of it follows, every page of the issue's events read (G3). A
    /// label set when the issue was opened has its event too, its actor the
    /// opener (G3, OWL-547). The account is the actor's, by the rule of a
    /// comment's author, unless the event was made through a GitHub App:
    /// an app user never admits (the [`Gesture`] contract), and the runner
    /// puts the ready label back through the app's token itself (`intake`'s
    /// return to the ready column), whose event names the token's holder. A
    /// label with no such event is an error asking to put it again, never
    /// the issue's opener: anyone opens an issue on a public repository.
    ///
    /// A person's gesture carries the issue's last text edit (OWL-557): the
    /// later of its last `renamed` event, a title edit, and GraphQL's
    /// `lastEditedAt`, a description edit, which no REST answer gives and
    /// which a title edit, a label or a comment does not move (checked live
    /// on 2026-11-14). An app's or a bot's gesture never admits, so its
    /// issue's edits are not read.
    pub(super) fn label_gesture(&self, id: &TicketId, admit: &Admit) -> Result<Gesture, Error> {
        let label = match admit {
            Admit::Label(label) | Admit::State(label) => label,
            Admit::Delegation => return Err(no_delegation()),
        };
        let number = number(id)?;
        let path = format!("{}/events", self.issue_path(number));
        let events: Vec<Event> = self.every(&path, &format!("the events of {id}"))?;
        let mut standing = None;
        let mut renamed = None;
        for event in events {
            match event.event.as_str() {
                "labeled" if event.of_label(label) => standing = Some(event),
                "unlabeled" if event.of_label(label) => standing = None,
                "renamed" => renamed = Some(timestamp(&event.created_at)?),
                _ => {}
            }
        }
        let Some(event) = standing else {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "no `labeled` event of {label:?} stands on {id}: GitHub records none of the \
                     label as it is, so put the label on the issue again"
                ),
            ));
        };
        let Event {
            actor,
            performed_via_github_app,
            created_at,
            ..
        } = event;
        let account = match (performed_via_github_app, actor) {
            (Some(app), actor) => Author::Other {
                name: format!(
                    "{} through the GitHub App {}",
                    actor.map_or_else(|| "unknown".to_owned(), |actor| actor.login),
                    app.slug.as_deref().unwrap_or("unknown")
                ),
            },
            (None, Some(actor)) => actor.author(),
            (None, None) => Author::unknown(),
        };
        let edited_at = match account {
            Author::Account(_) => renamed.max(self.last_edited(number, id)?),
            Author::Other { .. } => None,
        };
        Ok(Gesture {
            account,
            at: Some(timestamp(&created_at)?),
            edited_at,
        })
    }

    /// GraphQL's `lastEditedAt` of issue `number`: the last edit of its
    /// description, `None` when never edited (checked live on 2026-11-14,
    /// OWL-557). An issue GraphQL does not answer is an error, never one
    /// that was not edited.
    fn last_edited(&self, number: u64, id: &TicketId) -> Result<Option<Timestamp>, Error> {
        #[derive(Deserialize)]
        struct Data {
            repository: Option<Repository>,
        }
        #[derive(Deserialize)]
        struct Repository {
            issue: Option<Edited>,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Edited {
            last_edited_at: Option<String>,
        }
        let repo = self.forge.repo();
        let variables = json!({ "owner": repo.owner(), "name": repo.name(), "number": number });
        let data: Data = self
            .forge
            .tracker_query(LAST_EDITED_QUERY, variables)
            .map_err(tracker_error)?;
        let Some(issue) = data.repository.and_then(|repository| repository.issue) else {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("GitHub's GraphQL API answered no issue {id}, so its last edit is unknown"),
            ));
        };
        issue.last_edited_at.as_deref().map(timestamp).transpose()
    }

    /// The blockers of issue `number` by GitHub's `blocked_by` list, never
    /// its summary, which counts no blocker of another repository (G4); once
    /// each, in GitHub's order.
    fn blockers(&self, number: u64) -> Result<Vec<Blocker>, Error> {
        let mut blockers: Vec<Blocker> = Vec::new();
        for issue in self.dependencies(number, "blocked_by")? {
            let blocker = Blocker {
                id: self.dependency_id(&issue)?,
                state: self.dependency_state(&issue),
            };
            if !blockers.iter().any(|known| known.id == blocker.id) {
                blockers.push(blocker);
            }
        }
        Ok(blockers)
    }

    /// The issues of `number`'s dependency list `which`, `blocked_by` or
    /// `blocking`, every page read (G4).
    fn dependencies(&self, number: u64, which: &str) -> Result<Vec<Issue>, Error> {
        let path = format!("{}/dependencies/{which}", self.issue_path(number));
        self.every(&path, &format!("the `{which}` list of #{number}"))
    }

    /// Whether `issue`, from a dependency list, is one of this repository's.
    fn ours(&self, issue: &Issue) -> bool {
        let expected = format!("{REST_API}{}", self.repo_path());
        issue.repository_url.eq_ignore_ascii_case(&expected)
    }

    /// The id of a dependency: its number for one of this repository's,
    /// else [`foreign_id`].
    fn dependency_id(&self, issue: &Issue) -> Result<TicketId, Error> {
        if self.ours(issue) {
            issue_id(issue.number)
        } else {
            Ok(foreign_id(&issue.repository_url, issue.number))
        }
    }

    /// The state of a dependency: an issue of another repository holds,
    /// whatever it shows (G4), as an unknown identifier does.
    fn dependency_state(&self, issue: &Issue) -> Option<StateType> {
        if self.ours(issue) {
            state_type(issue)
        } else {
            None
        }
    }

    /// A dependency as a [`Linked`] ticket: its title, and its state named
    /// `open` or `closed`.
    fn linked(&self, issue: Issue) -> Result<Linked, Error> {
        let kind = self.dependency_state(&issue);
        Ok(Linked {
            id: self.dependency_id(&issue)?,
            state: Some(LinkedState {
                name: issue.state.clone(),
                kind,
            }),
            title: Some(issue.title),
        })
    }

    /// The issue, its state and description, and its blockers.
    pub(super) fn issue_relations(&self, id: &TicketId) -> Result<Relations, Error> {
        let (number, issue) = self.issue(id)?;
        Ok(Relations {
            id: id.clone(),
            state: state_type(&issue),
            description: issue.body.unwrap_or_default(),
            blockers: self.blockers(number)?,
        })
    }

    /// Its `blocked_by` and `blocking` lists; GitHub has no relation
    /// without an order, so `related` is empty.
    pub(super) fn issue_links(&self, id: &TicketId) -> Result<Links, Error> {
        let (number, _) = self.issue(id)?;
        let mut links = Links::default();
        for (which, list) in [
            ("blocked_by", &mut links.blocked_by),
            ("blocking", &mut links.blocks),
        ] {
            for issue in self.dependencies(number, which)? {
                let linked = self.linked(issue)?;
                if !list.iter().any(|known| known.id == linked.id) {
                    list.push(linked);
                }
            }
        }
        Ok(links)
    }

    /// Writes that the issue of database id `blocker` blocks issue
    /// `blocked` (G4: `issues=write`).
    fn write_blocked_by(&self, blocked: u64, blocker: u64) -> Result<(), Error> {
        let path = format!("{}/dependencies/blocked_by", self.issue_path(blocked));
        self.forge
            .rest_write(Verb::Post, &path, &json!({ "issue_id": blocker }))
            .map(|_| ())
            .map_err(tracker_error)
    }

    /// `blocker` blocks `blocked`, unless `blocked`'s list already says so.
    /// A write GitHub refuses is read again: a relation there since, written
    /// by a person or a write whose answer was lost, is what was asked.
    pub(super) fn write_block(&self, blocker: &TicketId, blocked: &TicketId) -> Result<(), Error> {
        let target = number(blocked)?;
        let there = |this: &Self| -> Result<bool, Error> {
            Ok(this.blockers(target)?.iter().any(|b| b.id == *blocker))
        };
        if there(self)? {
            return Ok(());
        }
        let (_, issue) = self.issue(blocker)?;
        match self.write_blocked_by(target, issue.id) {
            Ok(()) => Ok(()),
            Err(error) => match there(self) {
                Ok(true) => Ok(()),
                _ => Err(error),
            },
        }
    }

    /// Opens the follow-up as an issue with the [`PROPOSED_LABEL`] (section
    /// 6's fallback), then, when it needs the parent's code, writes that the
    /// parent blocks it (G4). A ready label that is the proposals' label is
    /// refused before anything is read or written. Once the issue exists,
    /// what goes wrong is a problem of [`Filed`].
    pub(super) fn open_proposal(&self, proposal: &Proposal<'_>) -> Result<Filed, Error> {
        if proposal.ready.eq_ignore_ascii_case(PROPOSED_LABEL) {
            return Err(Error::new(
                ErrorKind::Other,
                format!(
                    "the ready label is {PROPOSED_LABEL:?}, so a proposal filed with it would \
                     enter the queue without a person: refused"
                ),
            ));
        }
        let (_, parent) = self.issue(proposal.parent)?;
        let text = self
            .forge
            .rest_write(
                Verb::Post,
                &self.issues_path(),
                &json!({
                    "title": proposal.title,
                    "body": proposal.description,
                    "labels": [PROPOSED_LABEL],
                }),
            )
            .map_err(tracker_error)?;
        let created: Issue = parse(&text, &"the proposal just filed")?;
        let id = issue_id(created.number)?;
        let labelled = created
            .labels
            .iter()
            .any(|l| l.name.eq_ignore_ascii_case(PROPOSED_LABEL));
        let mut problems = Vec::new();
        if !labelled {
            problems.push(Error::new(
                ErrorKind::Other,
                format!(
                    "GitHub filed #{} without the label {PROPOSED_LABEL:?}",
                    created.number
                ),
            ));
        }
        if proposal.blocked_by_parent
            && let Err(error) = self.write_blocked_by(created.number, parent.id)
        {
            problems.push(Error::new(
                error.kind,
                format!(
                    "the relation that #{} blocks #{} was not written: {error}",
                    proposal.parent, created.number
                ),
            ));
        }
        Ok(Filed {
            id,
            state: if labelled { PROPOSED_LABEL } else { "open" }.to_owned(),
            problems,
        })
    }

    /// The issues carrying the [`PROPOSED_LABEL`], open or closed, whose
    /// title is `title` in any case or whose body holds `marker`, each once.
    /// Unlike Linear's search, a closed one counts whatever its reason:
    /// GitHub's default close is `completed`, so a proposal a person
    /// declined with it would otherwise be filed again.
    pub(super) fn find_proposals(&self, title: &str, marker: &str) -> Result<Vec<Found>, Error> {
        if title.trim().is_empty() || marker.trim().is_empty() {
            return Err(Error::new(
                ErrorKind::Other,
                "a proposal is searched by a title and a marker that are not blank",
            ));
        }
        let path = format!(
            "{}?state=all&labels={}",
            self.issues_path(),
            encode(PROPOSED_LABEL)
        );
        let issues: Vec<Issue> = self.every(&path, "the proposals")?;
        let mut found: Vec<Found> = Vec::new();
        for issue in issues {
            if issue.pull_request.is_some() {
                continue;
            }
            let description = issue.body.unwrap_or_default();
            if !issue.title.eq_ignore_ascii_case(title) && !description.contains(marker) {
                continue;
            }
            let id = issue_id(issue.number)?;
            if !found.iter().any(|f| f.id == id) {
                found.push(Found { id, description });
            }
        }
        Ok(found)
    }
}

/// The ticket id of issue `number`.
fn issue_id(number: u64) -> Result<TicketId, Error> {
    TicketId::new(number.to_string()).map_err(|_| {
        Error::new(
            ErrorKind::Other,
            format!("GitHub answered an issue number that does not read: {number}"),
        )
    })
}

/// The id a dependency of another repository is named by in a wait (G4):
/// `<owner>-<name>-<n>`, every character a ticket id cannot hold replaced by
/// `_`, or `elsewhere-<n>` when that is too long. Never a number alone, so
/// it is never one of this repository's tickets, and nothing it names is
/// read on the tracker.
fn foreign_id(repository_url: &str, number: u64) -> TicketId {
    let repo = repository_url
        .split_once("/repos/")
        .map_or("elsewhere", |(_, repo)| repo);
    let name: String = format!("{repo}-{number}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else if c == '/' {
                '-'
            } else {
                '_'
            }
        })
        .collect();
    TicketId::new(name)
        .or_else(|_| TicketId::new(format!("elsewhere-{number}")))
        .expect("`elsewhere-<n>` is a ticket id")
}

#[cfg(test)]
mod tests;
