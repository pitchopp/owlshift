//! GitHub's Issues API as the GitHub Issues tracker reads and writes it
//! (OWL-536): a repository's issues held in memory, served over the REST
//! paths the adapter calls, each answer shaped from the exchanges recorded
//! live on the sandbox (`owlshift-adapters/tests/fixtures/github-tracker/
//! drain.json`): an issue, an event and a comment as GitHub gave them, their
//! fields set from the state held here.
//!
//! What GitHub does on its own is played too: a label added through the
//! token gives a `labeled` event marked as made through the app, a removal an
//! `unlabeled` one with no mark (G3); a pull request whose body ends with
//! `Closes #n`, once merged, closes issue `n` as completed
//! ([`FakeIssues::merged`], G5).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

use owlshift_adapters::graphql::{Response, RestAnswer, Transport};

/// The exchanges recorded live, whose answers shape the fake's.
const RECORDED: &str =
    include_str!("../../owlshift-adapters/tests/fixtures/github-tracker/drain.json");

/// A GitHub account: its login and numeric id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub login: String,
    pub id: u64,
}

impl Account {
    pub fn new(login: &str, id: u64) -> Self {
        Self {
            login: login.to_owned(),
            id,
        }
    }

    fn json(&self) -> Value {
        json!({ "login": self.login, "id": self.id, "type": "User" })
    }
}

/// One issue as the fake holds it.
#[derive(Clone, Debug)]
pub struct Issue {
    pub title: String,
    pub body: String,
    pub author: Account,
    pub assignee: Option<Account>,
    pub labels: Vec<String>,
    /// `open` or `closed`.
    pub state: String,
    pub state_reason: Option<String>,
    /// The numbers of the issues that block it, in the order written.
    pub blocked_by: Vec<u64>,
    created_at: Timestamp,
    events: Vec<Value>,
    comments: Vec<Value>,
}

struct State {
    repo: String,
    /// The account the token belongs to: what the tracker writes as.
    viewer: Account,
    issues: BTreeMap<u64, Issue>,
    /// The time of the next write, a second after the last.
    now: Timestamp,
    next_id: u64,
    /// Every call, `GET <path>`, `POST <path> <body>` or `DELETE <path>`.
    calls: Vec<String>,
}

/// The fake, a handle shared between the tracker and the test.
#[derive(Clone)]
pub struct FakeIssues {
    state: Arc<Mutex<State>>,
}

/// A recorded answer: the first exchange whose path holds `path`.
fn recorded(path: &str) -> Value {
    let fixture: Value = serde_json::from_str(RECORDED).expect("the recorded drain");
    let exchanges = fixture["exchanges"].as_array().expect("its exchanges");
    let found = exchanges
        .iter()
        .find(|e| e["request"]["path"].as_str().is_some_and(|p| p.contains(path)))
        .unwrap_or_else(|| panic!("no recorded exchange of {path}"));
    let response = &found["response"];
    match response.as_array() {
        Some(items) => items[0].clone(),
        None => response.clone(),
    }
}

impl FakeIssues {
    /// The issues of `repo` (`owner/name`), written through `viewer`'s token,
    /// the clock starting at `start`.
    pub fn new(repo: &str, viewer: Account, start: Timestamp) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                repo: repo.to_owned(),
                viewer,
                issues: BTreeMap::new(),
                now: start,
                next_id: 1,
                calls: Vec::new(),
            })),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Opens issue `number` as `author`, assigned to `assignee`, each label
    /// of `labels` put by its account (a `labeled` event, G3).
    pub fn open(
        &self,
        number: u64,
        title: &str,
        body: &str,
        author: &Account,
        assignee: Option<&Account>,
        labels: &[(&str, &Account)],
    ) {
        let mut state = self.state();
        let at = state.tick();
        state.issues.insert(
            number,
            Issue {
                title: title.to_owned(),
                body: body.to_owned(),
                author: author.clone(),
                assignee: assignee.cloned(),
                labels: Vec::new(),
                state: "open".to_owned(),
                state_reason: None,
                blocked_by: Vec::new(),
                created_at: at,
                events: Vec::new(),
                comments: Vec::new(),
            },
        );
        for (label, by) in labels {
            state.label(number, label, by, false);
        }
    }

    /// Writes that `blocker` blocks `blocked`, as a person does on GitHub.
    pub fn block(&self, blocker: u64, blocked: u64) {
        self.state()
            .issues
            .get_mut(&blocked)
            .expect("the blocked issue")
            .blocked_by
            .push(blocker);
    }

    /// What GitHub does once a pull request whose body is `body` merges: the
    /// issue its last line closes (`Closes #n`) is closed as completed (G5).
    /// The issue closed, if any.
    pub fn merged(&self, body: &str) -> Option<u64> {
        let last = body.trim_end().lines().last()?.trim();
        let number: u64 = last.strip_prefix("Closes #")?.parse().ok()?;
        let mut state = self.state();
        let issue = state.issues.get_mut(&number)?;
        issue.state = "closed".to_owned();
        issue.state_reason = Some("completed".to_owned());
        Some(number)
    }

    /// Issue `number` as it is now.
    pub fn issue(&self, number: u64) -> Issue {
        self.state().issues[&number].clone()
    }

    /// The numbers of every issue.
    pub fn numbers(&self) -> Vec<u64> {
        self.state().issues.keys().copied().collect()
    }

    /// Every call made so far.
    pub fn calls(&self) -> Vec<String> {
        self.state().calls.clone()
    }
}

impl State {
    fn tick(&mut self) -> Timestamp {
        let at = self.now;
        self.now = at.checked_add(SignedDuration::from_secs(1)).unwrap();
        at
    }

    fn api(&self) -> String {
        format!("https://api.github.com/repos/{}", self.repo)
    }

    /// Puts `label` on issue `number` as `by`, through the app's token when
    /// `app` is set (G3).
    fn label(&mut self, number: u64, label: &str, by: &Account, app: bool) {
        let at = self.tick();
        let event = self.event("labeled", label, by, app, at);
        let issue = self.issues.get_mut(&number).expect("the issue");
        if !issue.labels.iter().any(|l| l.eq_ignore_ascii_case(label)) {
            issue.labels.push(label.to_owned());
        }
        issue.events.push(event);
    }

    fn unlabel(&mut self, number: u64, label: &str) -> bool {
        let at = self.tick();
        let viewer = self.viewer.clone();
        let event = self.event("unlabeled", label, &viewer, false, at);
        let issue = self.issues.get_mut(&number).expect("the issue");
        let before = issue.labels.len();
        issue.labels.retain(|l| !l.eq_ignore_ascii_case(label));
        let removed = issue.labels.len() < before;
        if removed {
            issue.events.push(event);
        }
        removed
    }

    fn event(&mut self, kind: &str, label: &str, by: &Account, app: bool, at: Timestamp) -> Value {
        let mut event = recorded("/events?");
        self.next_id += 1;
        event["id"] = json!(self.next_id);
        event["event"] = json!(kind);
        event["actor"] = by.json();
        event["label"] = json!({ "name": label, "color": "ededed" });
        event["created_at"] = json!(at.to_string());
        event["performed_via_github_app"] = if app {
            json!({ "slug": "owl-shift" })
        } else {
            Value::Null
        };
        event
    }

    /// Issue `number`'s answer, shaped as the recorded one.
    fn answer(&self, number: u64) -> Value {
        let issue = &self.issues[&number];
        let mut answer = recorded("/issues/14");
        let url = format!("{}/issues/{number}", self.api());
        for (key, value) in [
            ("id", json!(100_000 + number)),
            ("number", json!(number)),
            ("url", json!(url)),
            ("repository_url", json!(self.api())),
            ("title", json!(issue.title)),
            ("body", json!(issue.body)),
            ("user", issue.author.json()),
            (
                "assignees",
                json!(issue.assignee.iter().map(Account::json).collect::<Vec<_>>()),
            ),
            (
                "labels",
                json!(
                    issue
                        .labels
                        .iter()
                        .map(|name| json!({ "name": name, "color": "ededed" }))
                        .collect::<Vec<_>>()
                ),
            ),
            ("state", json!(issue.state)),
            ("state_reason", json!(issue.state_reason)),
            ("created_at", json!(issue.created_at.to_string())),
        ] {
            answer[key] = value;
        }
        answer
    }

    /// The open (or every, for `all`) issue carrying each of `labels`.
    fn list(&self, state: &str, labels: &[String]) -> Value {
        let found: Vec<Value> = self
            .issues
            .iter()
            .filter(|(_, issue)| state == "all" || issue.state == state)
            .filter(|(_, issue)| {
                labels
                    .iter()
                    .all(|wanted| issue.labels.iter().any(|l| l.eq_ignore_ascii_case(wanted)))
            })
            .map(|(number, _)| self.answer(*number))
            .collect();
        json!(found)
    }

    fn get(&mut self, path: &str) -> (u16, Value) {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let params: BTreeMap<&str, String> = query
            .split('&')
            .filter_map(|pair| pair.split_once('='))
            .map(|(key, value)| (key, decode(value)))
            .collect();
        if params.get("page").is_some_and(|page| page != "1") {
            return (200, json!([]));
        }
        let prefix = format!("repos/{}/issues", self.repo);
        let Some(rest) = route.strip_prefix(&prefix) else {
            return not_found();
        };
        if rest.is_empty() {
            let labels: Vec<String> = params
                .get("labels")
                .map(|labels| labels.split(',').map(str::to_owned).collect())
                .unwrap_or_default();
            let state = params.get("state").map_or("open", String::as_str);
            return (200, self.list(state, &labels));
        }
        let parts: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
        let Some(number) = parts.first().and_then(|n| n.parse::<u64>().ok()) else {
            return not_found();
        };
        let Some(issue) = self.issues.get(&number) else {
            return not_found();
        };
        match parts[1..] {
            [] => (200, self.answer(number)),
            ["events"] => (200, json!(issue.events)),
            ["comments"] => (200, json!(issue.comments)),
            ["dependencies", "blocked_by"] => {
                let blockers: Vec<Value> =
                    issue.blocked_by.iter().map(|n| self.answer(*n)).collect();
                (200, json!(blockers))
            }
            ["dependencies", "blocking"] => {
                let blocking: Vec<Value> = self
                    .issues
                    .iter()
                    .filter(|(_, other)| other.blocked_by.contains(&number))
                    .map(|(n, _)| self.answer(*n))
                    .collect();
                (200, json!(blocking))
            }
            _ => not_found(),
        }
    }

    fn post(&mut self, path: &str, body: &Value) -> (u16, Value) {
        let prefix = format!("repos/{}/issues", self.repo);
        let Some(rest) = path.strip_prefix(&prefix) else {
            return not_found();
        };
        if rest.is_empty() {
            let number = self.issues.keys().max().map_or(1, |n| n + 1);
            let viewer = self.viewer.clone();
            let at = self.tick();
            self.issues.insert(
                number,
                Issue {
                    title: body["title"].as_str().unwrap_or_default().to_owned(),
                    body: body["body"].as_str().unwrap_or_default().to_owned(),
                    author: viewer.clone(),
                    assignee: None,
                    labels: Vec::new(),
                    state: "open".to_owned(),
                    state_reason: None,
                    blocked_by: Vec::new(),
                    created_at: at,
                    events: Vec::new(),
                    comments: Vec::new(),
                },
            );
            for label in body["labels"].as_array().into_iter().flatten() {
                self.label(number, label.as_str().unwrap_or_default(), &viewer, true);
            }
            return (201, self.answer(number));
        }
        let parts: Vec<&str> = rest.trim_start_matches('/').split('/').collect();
        let Some(number) = parts.first().and_then(|n| n.parse::<u64>().ok()) else {
            return not_found();
        };
        if !self.issues.contains_key(&number) {
            return not_found();
        }
        match parts[1..] {
            ["comments"] => {
                let at = self.tick();
                self.next_id += 1;
                let mut comment = recorded("/comments?");
                for (key, value) in [
                    ("id", json!(self.next_id)),
                    ("issue_url", json!(format!("{}/issues/{number}", self.api()))),
                    ("user", self.viewer.json()),
                    ("created_at", json!(at.to_string())),
                    ("updated_at", json!(at.to_string())),
                    ("body", body["body"].clone()),
                    ("reactions", json!({ "total_count": 0 })),
                ] {
                    comment[key] = value;
                }
                let issue = self.issues.get_mut(&number).expect("the issue");
                issue.comments.push(comment.clone());
                (201, comment)
            }
            ["labels"] => {
                let viewer = self.viewer.clone();
                for label in body["labels"].as_array().into_iter().flatten() {
                    self.label(number, label.as_str().unwrap_or_default(), &viewer, true);
                }
                (200, json!(self.issues[&number].labels))
            }
            ["dependencies", "blocked_by"] => {
                let id = body["issue_id"].as_u64().unwrap_or_default();
                let Some(blocker) = id.checked_sub(100_000) else {
                    return not_found();
                };
                let issue = self.issues.get_mut(&number).expect("the issue");
                if !issue.blocked_by.contains(&blocker) {
                    issue.blocked_by.push(blocker);
                }
                (201, self.answer(blocker))
            }
            _ => not_found(),
        }
    }

    fn delete(&mut self, path: &str) -> (u16, Value) {
        let prefix = format!("repos/{}/issues/", self.repo);
        let Some((number, label)) = path
            .strip_prefix(&prefix)
            .and_then(|rest| rest.split_once("/labels/"))
        else {
            return not_found();
        };
        let Ok(number) = number.parse::<u64>() else {
            return not_found();
        };
        if self.issues.contains_key(&number) && self.unlabel(number, &decode(label)) {
            (200, json!(self.issues[&number].labels))
        } else {
            not_found()
        }
    }
}

fn not_found() -> (u16, Value) {
    (404, json!({ "message": "Not Found" }))
}

/// `text` with its percent-encoded bytes decoded.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = text
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Transport for FakeIssues {
    fn send(&self, request: &Value) -> Result<Response, String> {
        panic!("the Issues fake answers no GraphQL: {request}")
    }

    fn get(&self, path: &str) -> Result<RestAnswer, String> {
        let mut state = self.state();
        state.calls.push(format!("GET {path}"));
        let (status, body) = state.get(path);
        Ok(RestAnswer::new(status, body.to_string()))
    }

    fn post(&self, path: &str, body: &Value) -> Result<Response, String> {
        let mut state = self.state();
        state.calls.push(format!("POST {path} {body}"));
        let (status, answer) = state.post(path, body);
        Ok(Response::new(status, answer.to_string()))
    }

    fn delete(&self, path: &str) -> Result<Response, String> {
        let mut state = self.state();
        state.calls.push(format!("DELETE {path}"));
        let (status, answer) = state.delete(path);
        Ok(Response::new(status, answer.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fake's answers are shaped from the recorded ones: the issue's
    /// keys are all there, and a merged body closes its issue.
    #[test]
    fn answers_are_shaped_from_the_recording_and_a_merge_closes() {
        let ann = Account::new("ann", 1);
        let fake = FakeIssues::new("demo/project", ann.clone(), "2026-11-12T15:00:00Z".parse().unwrap());
        fake.open(1, "One", "Body", &ann, Some(&ann), &[("agent:ready", &ann)]);
        let answer = fake.get("repos/demo/project/issues/1").unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut { answer.body }, &mut text).unwrap();
        let read: Value = serde_json::from_str(&text).unwrap();
        for key in recorded("/issues/14").as_object().unwrap().keys() {
            assert!(read.get(key).is_some(), "{key}");
        }
        assert_eq!(read["labels"][0]["name"], "agent:ready");
        assert_eq!(fake.merged("Says hello.\n\nCloses #1"), Some(1));
        assert_eq!(fake.issue(1).state_reason.as_deref(), Some("completed"));
        assert_eq!(fake.merged("Says hello."), None);
    }
}
