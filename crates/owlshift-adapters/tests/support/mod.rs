//! Recorded Linear exchanges: replaying them in tests, and recording them
//! again against the live API.
//!
//! A fixture is a JSON file under `tests/fixtures/linear/`: the exchanges one
//! test makes, in order, each holding the request body, the HTTP status and
//! the response body. Headers are never recorded, so no fixture holds the API
//! key. Recording pseudonymizes every Linear account (`id`, `displayName`)
//! and reads no e-mail address at all.
//!
//! To record again, run the ignored recorder tests with the key in
//! `LINEAR_API_KEY`, or in a dotenv file named by `LINEAR_ENV_FILE` (never
//! on the command line): `cargo test -p owlshift-adapters -- --ignored
//! record_`. Only
//! `record_conformance_fixture` writes to Linear, and only when
//! `OWLSHIFT_RECORD_WRITES` names the issue it may comment on.

#![allow(dead_code)]

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use owlshift_adapters::tracker::linear::{ApiKey, HttpTransport, Response, Transport};

/// One request and its answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Exchange {
    pub request: Value,
    pub status: u16,
    pub response: Value,
    /// Set on an exchange made up rather than recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthesized: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Fixture {
    /// When and how it was recorded.
    pub recorded: String,
    pub exchanges: Vec<Exchange>,
}

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/linear")
}

pub fn load(name: &str) -> Fixture {
    let path = fixtures_dir().join(name);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A transport that answers from a fixture and checks that every request is
/// the recorded one, in order. A changed query fails here until the fixture
/// is recorded again.
#[derive(Clone)]
pub struct Replay {
    name: String,
    left: Arc<Mutex<VecDeque<Exchange>>>,
}

impl Replay {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            left: Arc::new(Mutex::new(load(name).exchanges.into())),
        }
    }

    /// Fails unless every recorded exchange was used.
    pub fn assert_done(&self) {
        let left = self.left.lock().unwrap().len();
        assert_eq!(
            left, 0,
            "{}: {left} recorded exchanges were not used",
            self.name
        );
    }
}

impl Transport for Replay {
    fn send(&self, request: &Value) -> Result<Response, String> {
        let next = self.left.lock().unwrap().pop_front();
        let next = next
            .unwrap_or_else(|| panic!("{}: no recorded exchange left for {request}", self.name));
        assert_eq!(
            request, &next.request,
            "{}: the request differs from the recorded one; record the fixture again",
            self.name
        );
        Ok(Response {
            status: next.status,
            body: next.response.to_string(),
        })
    }
}

/// The key for recording: `LINEAR_API_KEY`, or the `LINEAR_API_KEY=` line of
/// the dotenv file that `LINEAR_ENV_FILE` names, which keeps the key out of
/// the environment of cargo and everything it spawns.
pub fn live_key() -> ApiKey {
    if let Ok(key) = std::env::var("LINEAR_API_KEY") {
        return ApiKey::new(key);
    }
    let file = std::env::var("LINEAR_ENV_FILE")
        .expect("recording needs LINEAR_API_KEY, or LINEAR_ENV_FILE naming a dotenv file");
    let text = fs::read_to_string(&file).unwrap_or_else(|e| panic!("{file}: {e}"));
    let key = text
        .lines()
        .find_map(|line| line.strip_prefix("LINEAR_API_KEY="))
        .unwrap_or_else(|| panic!("{file} has no LINEAR_API_KEY= line"));
    ApiKey::new(key.trim())
}

/// Sends one request to the live API outside any recording.
pub fn live_query(key: &ApiKey, query: &str) -> Value {
    let response = HttpTransport::new(key.clone())
        .send(&json!({ "query": query }))
        .unwrap();
    serde_json::from_str(&response.body).unwrap()
}

/// Recording must only ever touch the Owlshift workspace.
pub fn assert_owlshift_workspace(key: &ApiKey) {
    let answer = live_query(key, "{ organization { urlKey } }");
    assert_eq!(
        answer["data"]["organization"]["urlKey"], "owlshift",
        "recording runs against the Owlshift workspace only"
    );
}

/// A transport that sends to the live API and keeps every exchange.
///
/// With `synthesize` set to an issue, a comment posted on that issue is not
/// sent: the answer is made up from the request, and later comment reads of
/// that issue show it. That produces a fixture of the whole conformance run
/// without writing to Linear, flagged exchange by exchange.
#[derive(Clone)]
pub struct Recorder {
    live: Arc<HttpTransport>,
    log: Arc<Mutex<Vec<Exchange>>>,
    synthesize: Option<Synthesize>,
}

#[derive(Clone)]
struct Synthesize {
    issue: String,
    author: Value,
    posted: Arc<Mutex<Vec<Value>>>,
}

impl Recorder {
    pub fn new(key: &ApiKey) -> Self {
        Self {
            live: Arc::new(HttpTransport::new(key.clone())),
            log: Arc::default(),
            synthesize: None,
        }
    }

    /// Makes up the comments posted on `issue`, attributed to the key's own
    /// account, instead of posting them.
    pub fn synthesizing_posts_on(mut self, key: &ApiKey, issue: &str) -> Self {
        let viewer = live_query(key, "{ viewer { id displayName } }");
        self.synthesize = Some(Synthesize {
            issue: issue.to_owned(),
            author: viewer["data"]["viewer"].clone(),
            posted: Arc::default(),
        });
        self
    }

    /// Writes what was recorded, pseudonymized.
    pub fn write(&self, name: &str, recorded: &str) {
        let mut exchanges = self.log.lock().unwrap().clone();
        let mut people = BTreeMap::new();
        for exchange in &mut exchanges {
            pseudonymize(&mut exchange.response, &mut people);
        }
        let fixture = Fixture {
            recorded: recorded.to_owned(),
            exchanges,
        };
        let text = serde_json::to_string_pretty(&fixture).unwrap() + "\n";
        fs::create_dir_all(fixtures_dir()).unwrap();
        fs::write(fixtures_dir().join(name), text).unwrap();
    }

    fn made_up(&self, request: &Value) -> Option<Exchange> {
        let synthesize = self.synthesize.as_ref()?;
        let variables = &request["variables"];
        let query = request["query"].as_str().unwrap_or_default();
        if query.starts_with("mutation PostComment") && variables["issueId"] == synthesize.issue {
            let mut posted = synthesize.posted.lock().unwrap();
            let comment = json!({
                "id": format!("00000000-0000-4000-8000-00000000c{:03}", posted.len() + 1),
                "body": variables["body"],
                "createdAt": Timestamp::now().round(jiff::Unit::Millisecond).unwrap().to_string(),
                "editedAt": null,
                "user": synthesize.author,
                "botActor": null,
                "externalUser": null,
            });
            posted.push(comment.clone());
            return Some(Exchange {
                request: request.clone(),
                status: 200,
                response: json!({ "data": { "commentCreate": { "success": true, "comment": comment } } }),
                synthesized: Some("not posted: shaped like the recorded comments".to_owned()),
            });
        }
        None
    }
}

impl Transport for Recorder {
    fn send(&self, request: &Value) -> Result<Response, String> {
        let exchange = match self.made_up(request) {
            Some(exchange) => exchange,
            None => {
                if let Some(s) = &self.synthesize {
                    let query = request["query"].as_str().unwrap_or_default();
                    assert!(
                        !(query.starts_with("mutation")
                            && request["variables"]["issueId"] == s.issue),
                        "refusing to write to {} without OWLSHIFT_RECORD_WRITES",
                        s.issue
                    );
                }
                let answer = self.live.send(request)?;
                let mut response: Value =
                    serde_json::from_str(&answer.body).unwrap_or(Value::String(answer.body));
                let mut synthesized = None;
                if let Some(s) = &self.synthesize {
                    let query = request["query"].as_str().unwrap_or_default();
                    let posted = s.posted.lock().unwrap();
                    if query.starts_with("query Comments")
                        && request["variables"]["id"] == s.issue
                        && !posted.is_empty()
                    {
                        let nodes = &mut response["data"]["issue"]["comments"]["nodes"];
                        let nodes = nodes.as_array_mut().expect("a comment page");
                        // Linear lists newest first.
                        for comment in posted.iter() {
                            nodes.insert(0, comment.clone());
                        }
                        synthesized = Some("recorded, plus the made-up comment".to_owned());
                    }
                }
                Exchange {
                    request: request.clone(),
                    status: answer.status,
                    response,
                    synthesized,
                }
            }
        };
        let answer = Response {
            status: exchange.status,
            body: exchange.response.to_string(),
        };
        self.log.lock().unwrap().push(exchange);
        Ok(answer)
    }
}

/// Replaces every Linear account (an object with `id` and `displayName`) by
/// a stable pseudonym: `person-1` and a fixed fake id for the first account
/// met, and so on.
fn pseudonymize(value: &mut Value, people: &mut BTreeMap<String, usize>) {
    match value {
        Value::Object(map) => {
            let account = match (map.get("id"), map.contains_key("displayName")) {
                (Some(Value::String(id)), true) => Some(id.clone()),
                _ => None,
            };
            if let Some(id) = account {
                let next = people.len() + 1;
                let n = *people.entry(id).or_insert(next);
                map.insert(
                    "id".to_owned(),
                    json!(format!("00000000-0000-4000-8000-{n:012}")),
                );
                map.insert("displayName".to_owned(), json!(format!("person-{n}")));
            }
            for inner in map.values_mut() {
                pseudonymize(inner, people);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| pseudonymize(v, people)),
        _ => {}
    }
}
