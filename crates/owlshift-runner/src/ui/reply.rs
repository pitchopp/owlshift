//! Replies from the UI (P8 part 10, OWL-475, architecture section 5, "Replies
//! from the UI"): the decider answers a ticket's open questions, or approves
//! or changes its plan, from the ticket's page, and the reply is posted
//! through the operator's own credential for the project's tracker as the
//! decider's own comment, which the answer check reads as any reply on the
//! tracker.
//!
//! - *The key* ([`open_key`], [`Keys`]): on Linear the key `owlshift init`
//!   stored for the project, its own or the shared one (OWL-308), never the
//!   app's; on GitHub Issues the forge credential (OWL-552); and its
//!   account, read once per served project.
//! - *The offer* ([`section`]): the snapshot's `questions.reply`, whether the
//!   form is offered, only when the key's account is the open ask's decider
//!   ([`owlshift_core::floor::same_account`]), and otherwise who the decider
//!   is and why the form is not offered.
//! - *The decider's name* ([`Keys::decider_name`], OWL-517): read through
//!   the key on a thread of its own, once per project and account, the page
//!   request never waiting on it; the account id stands for it meanwhile.
//! - *The composition* ([`compose`]): the picked options and the texts in the
//!   answer check's form (`Q1: B`, `ok` for every recommendation), with a
//!   last line `go` unless "count it now" is unticked.
//! - *The post* ([`send`]): through the Writer of the UI
//!   ([`Writer::for_ui`]), whose one write the floor allows is that reply.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use owlshift_adapters::forge::Repo;
#[cfg(test)]
use owlshift_adapters::tracker::linear::LinearTracker;
use owlshift_adapters::tracker::{Comment, KeyAccount, Tracker};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{Ask, Waiting};
use owlshift_contracts::result::{Question, option_index, option_letter};
use owlshift_core::floor::same_account;
use owlshift_core::reply::ends_with_go;
use serde_json::{Map, Value, json};

use crate::on_demand::{APPROVAL_CATEGORY, change_needs_words};
use crate::project::ProjectDirs;
use crate::ticket_ref::TicketRecord;
use crate::writer::{WriteError, Writer};

/// Reads the name the tracker shows for an account id, through a key:
/// `None` for an account it does not know.
pub type NameReader = Box<dyn Fn(&str) -> Result<Option<String>, String> + Send + Sync>;

/// The operator's tracker key, opened: its account, the adapter that posts
/// through it, and the read of another account's name through it.
pub struct Key {
    pub account: KeyAccount,
    pub tracker: Box<dyn Tracker + Send + Sync>,
    pub names: NameReader,
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Key")
            .field("account", &self.account)
            .finish_non_exhaustive()
    }
}

/// Opens a served project's key: [`open_key`] in production.
pub type KeyOpener = Arc<dyn Fn(&Path, &Repo) -> Result<Key, String> + Send + Sync>;

/// Where a served project's key stands.
#[derive(Debug)]
pub enum KeyState {
    /// Its account is being read.
    Reading,
    /// Read: the form may be offered.
    Ready(Key),
    /// It could not be read, and why: no form is offered.
    Unavailable(String),
}

/// How long a decider's name that could not be read stays unread before the
/// next need of it reads it again.
pub const NAME_RETRY: Duration = Duration::from_secs(5 * 60);

/// Where the name of a decider stands, by project and account.
#[derive(Debug)]
enum NameState {
    /// Being read.
    Reading,
    /// Read, kept while the UI runs: `None` for an account the tracker does
    /// not know.
    Known(Option<String>),
    /// Could not be read, at that instant.
    Failed(Instant),
}

/// The keys of the served projects, each read once, by lower-cased
/// `owner/repo`: those served when the UI starts at its start
/// ([`Keys::read_all`]), one served later at the first need of it
/// ([`Keys::state`]). Never read again while the UI runs; a failure is kept.
/// With them, the names of the deciders that are not the key's account
/// ([`Keys::decider_name`]).
pub struct Keys {
    opener: KeyOpener,
    states: Mutex<HashMap<String, Arc<KeyState>>>,
    names: Mutex<HashMap<(String, String), NameState>>,
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keys").finish_non_exhaustive()
    }
}

fn name(repo: &Repo) -> String {
    repo.to_string().to_ascii_lowercase()
}

impl Keys {
    pub fn new(opener: KeyOpener) -> Arc<Self> {
        Arc::new(Self {
            opener,
            states: Mutex::new(HashMap::new()),
            names: Mutex::new(HashMap::new()),
        })
    }

    fn names(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), NameState>> {
        self.names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The name of `account`, the decider of an open ask of `repo`, whose
    /// key is `key`, as far as it is read; never waits for it. Only a read
    /// key's, for an account that is not the key's own (whose name the key
    /// holds): the first need of it starts its read on a thread of its own,
    /// through the key, and answers `None`, as each need does while it is
    /// read. A name read, or an account the tracker does not know, is kept
    /// while the UI runs; a failed read is read again at the first need
    /// [`NAME_RETRY`] after it.
    pub fn decider_name(
        self: &Arc<Self>,
        repo: &Repo,
        key: &Arc<KeyState>,
        account: &str,
    ) -> Option<String> {
        self.decider_name_at(repo, key, account, Instant::now())
    }

    fn decider_name_at(
        self: &Arc<Self>,
        repo: &Repo,
        key: &Arc<KeyState>,
        account: &str,
        now: Instant,
    ) -> Option<String> {
        let KeyState::Ready(ready) = &**key else {
            return None;
        };
        if account.trim().is_empty() || same_account(&ready.account.id, account) {
            return None;
        }
        let entry = (name(repo), account.to_owned());
        {
            let mut names = self.names();
            match names.get(&entry) {
                Some(NameState::Known(known)) => return known.clone(),
                Some(NameState::Reading) => return None,
                Some(NameState::Failed(at)) if now.saturating_duration_since(*at) < NAME_RETRY => {
                    return None;
                }
                _ => {}
            }
            names.insert(entry.clone(), NameState::Reading);
        }
        let read = NameRead {
            keys: self.clone(),
            entry: entry.clone(),
            outcome: None,
        };
        let (key, account) = (key.clone(), account.to_owned());
        let spawned = std::thread::Builder::new()
            .name("ui-names".into())
            .spawn(move || {
                // The whole guard moves in, so it keeps the outcome when the
                // thread ends, by its end or by a panic.
                let mut read = read;
                if let KeyState::Ready(ready) = &*key {
                    read.outcome = Some(match (ready.names)(&account) {
                        Ok(known) => NameState::Known(known),
                        Err(_) => NameState::Failed(Instant::now()),
                    });
                }
            });
        if spawned.is_err() {
            self.names().insert(entry, NameState::Failed(now));
        }
        None
    }

    fn states(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<KeyState>>> {
        self.states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Marks `repo` as being read, unless it was marked before: whether this
    /// call is the one to read it.
    fn claim(&self, repo: &Repo) -> bool {
        let mut states = self.states();
        let key = name(repo);
        if states.contains_key(&key) {
            return false;
        }
        states.insert(key, Arc::new(KeyState::Reading));
        true
    }

    /// Reads `repo`'s key now, once its claim is taken, and keeps the
    /// outcome.
    fn read(&self, data: &Path, repo: &Repo) {
        let state = match (self.opener)(data, repo) {
            Ok(key) => KeyState::Ready(key),
            Err(why) => KeyState::Unavailable(why),
        };
        self.states().insert(name(repo), Arc::new(state));
    }

    /// Reads the keys of `repos` on a thread of their own, each once: what
    /// the UI does at its start for the projects served then.
    pub fn read_all(self: &Arc<Self>, data: &Path, repos: Vec<Repo>) {
        let claimed: Vec<Repo> = repos.into_iter().filter(|repo| self.claim(repo)).collect();
        if claimed.is_empty() {
            return;
        }
        let names: Vec<String> = claimed.iter().map(name).collect();
        let (keys, data) = (self.clone(), data.to_owned());
        let spawned = std::thread::Builder::new()
            .name("ui-keys".into())
            .spawn(move || {
                for repo in &claimed {
                    keys.read(&data, repo);
                }
            });
        if let Err(error) = spawned {
            let why = format!("the tracker keys could not be read: {error}");
            let mut states = self.states();
            for name in names {
                states.insert(name, Arc::new(KeyState::Unavailable(why.clone())));
            }
        }
    }

    /// The key of `repo` as it stands; a project not read yet, served since
    /// the UI started, is read now on a thread and is [`KeyState::Reading`]
    /// meanwhile.
    pub fn state(self: &Arc<Self>, data: &Path, repo: &Repo) -> Arc<KeyState> {
        if let Some(state) = self.states().get(&name(repo)) {
            return state.clone();
        }
        if self.claim(repo) {
            let (keys, data, repo) = (self.clone(), data.to_owned(), repo.clone());
            let spawned = std::thread::Builder::new()
                .name("ui-keys".into())
                .spawn(move || keys.read(&data, &repo));
            if let Err(error) = spawned {
                return Arc::new(KeyState::Unavailable(format!(
                    "the tracker key could not be read: {error}"
                )));
            }
        }
        self.states()
            .get(&name(repo))
            .cloned()
            .unwrap_or_else(|| Arc::new(KeyState::Reading))
    }
}

/// A decider's name being read: keeps its outcome when dropped, a failure
/// when there is none, so a read that panicked is read again in time.
struct NameRead {
    keys: Arc<Keys>,
    entry: (String, String),
    outcome: Option<NameState>,
}

impl Drop for NameRead {
    fn drop(&mut self) {
        let outcome = self
            .outcome
            .take()
            .unwrap_or_else(|| NameState::Failed(Instant::now()));
        self.keys
            .names()
            .insert(std::mem::take(&mut self.entry), outcome);
    }
}

/// The production opener: the credential the project's tracker kind says
/// ([`crate::tracker::Kind::reply_key`]), read from the system keychain, and
/// its account read once through it. On Linear that is the key `owlshift
/// init` stored for the project of `repo`, its own accounts or the shared
/// ones ([`crate::secrets::tracker`]), never the app's pair; on GitHub Issues
/// the forge credential. A reply goes through it because its account is the
/// operator's. A project whose tracker has no such credential, the Markdown
/// tracker, answers why there is none, before the keychain is asked.
pub fn open_key(data: &Path, repo: &Repo) -> Result<Key, String> {
    let config = super::ticket::served_config(&ProjectDirs::new(data, repo))?;
    crate::tracker::of(config.tracker.kind).reply_key(data, repo, &config, &super::personal_now())
}

/// The ask a ticket's newest record waits on: its latest, while its state
/// waits for answers. A ticket parked while questions waited gets none: an
/// answer alone does not restart it.
pub fn open_ask(record: &TicketRecord) -> Option<&Ask> {
    if record.state.waiting != Some(Waiting::NeedsInput) {
        return None;
    }
    record.questions.asks.last()
}

/// Whether `question` is the plan's approval question.
fn is_approval(question: &Question) -> bool {
    question.category == APPROVAL_CATEGORY
}

/// Why a key that could not be read offers no form: the UI reads each
/// key once, so the page says that a restart of the UI reads it again.
pub(crate) fn unusable(why: &str) -> String {
    format!(
        "the project's tracker key cannot be used: {why}. The UI reads it once: \
         restart the UI to read it again"
    )
}

/// The ticket's link on Linear, from the workspace the key read, with no
/// title slug; none for a workspace key that is not plain letters, digits,
/// `-` and `_`, so the page is never handed another address.
fn link(account: &KeyAccount, ticket: &TicketId) -> Option<String> {
    let plain = !account.workspace.is_empty()
        && account
            .workspace
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    plain.then(|| format!("https://linear.app/{}/issue/{}", account.workspace, ticket))
}

/// The snapshot's `questions.reply` of `record`, with `key` the project's
/// key as it stands: null when the ticket waits for no answer. `offered`
/// only when the key is read, no app's, and its account the open ask's
/// decider; `why_not` otherwise. The decider is named by the rule and the
/// account the ask keeps, the ref holding no name, and by the name the
/// tracker shows for that account: the key's own when it is the decider's,
/// otherwise what `names` has read of it so far ([`Keys::decider_name`]),
/// asked only of a read key that is not the decider's; null while none is.
/// `link` comes from the workspace the key read, so it is null until it is
/// read, and when it could not be.
pub fn section(
    record: &TicketRecord,
    key: &KeyState,
    ticket: &TicketId,
    names: &dyn Fn(&str) -> Option<String>,
) -> Value {
    let Some(ask) = open_ask(record) else {
        return Value::Null;
    };
    let approval = ask.questions.iter().any(is_approval);
    let mut words = Map::new();
    for question in &ask.questions {
        let letters: Vec<String> = if is_approval(question) {
            question
                .options
                .iter()
                .enumerate()
                .filter(|(_, option)| change_needs_words(option))
                .map(|(index, _)| option_letter(index))
                .collect()
        } else {
            Vec::new()
        };
        words.insert(question.id.to_string(), json!(letters));
    }
    let decider = &ask.decider;
    let mut decider_name = None;
    let (key_value, offered, why_not, link) = match key {
        KeyState::Reading => (
            json!({ "reading": true }),
            false,
            Some("the account of the project's tracker key is still being read".to_owned()),
            Value::Null,
        ),
        KeyState::Unavailable(why) => (
            json!({ "why": why }),
            false,
            Some(unusable(why)),
            Value::Null,
        ),
        KeyState::Ready(Key { account, .. }) => {
            decider_name = if same_account(&account.id, &decider.account) {
                Some(account.name.clone())
            } else {
                names(&decider.account)
            };
            let why_not = if account.app {
                Some("the project's tracker key is an app's, not an operator's".to_owned())
            } else if !same_account(&account.id, &decider.account) {
                Some(
                    "the project's tracker key is not the decider's: the reply is theirs to post"
                        .to_owned(),
                )
            } else {
                None
            };
            (
                json!({ "name": account.name, "account": account.id }),
                why_not.is_none(),
                why_not,
                json!(self::link(account, ticket)),
            )
        }
    };
    json!({
        "ask": ask.comment,
        "round": ask.round.get(),
        "kind": ask.kind,
        "approval": approval,
        "words": words,
        "decider": { "account": decider.account, "by": decider.by, "name": decider_name },
        "key": key_value,
        "offered": offered,
        "why_not": why_not,
        "link": link,
    })
}

/// What the form picked for a question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// An option, by its letter.
    Letter(String),
    /// The question's recommendation, whatever it is.
    Recommendation,
}

/// The form's answer to one question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The question's id in the ask, such as `Q1`.
    pub question: String,
    pub choice: Option<Choice>,
    /// The decider's words, possibly empty.
    pub text: String,
}

/// Why a form does not compose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposeError {
    /// No question was answered.
    NoAnswer,
    /// The ask has no question of this id.
    UnknownQuestion(String),
    /// The form answered this question twice.
    Repeated(String),
    /// The question has no option of this letter.
    NoSuchOption { question: String, letter: String },
    /// The recommendation was taken on a question that makes none.
    NoRecommendation(String),
    /// This option of the plan's approval changes it only with words, and
    /// none were written: the design run could not tell the change.
    ChangeWithoutText { question: String, letter: String },
    /// "Count it now" is unticked, yet the reply ends with the word go,
    /// which the answer check counts at once.
    EndsWithGo,
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAnswer => f.write_str("the reply answers no question"),
            Self::UnknownQuestion(id) => write!(f, "the open round has no question {id}"),
            Self::Repeated(id) => write!(f, "question {id} is answered twice"),
            Self::NoSuchOption { question, letter } => {
                write!(f, "question {question} has no option {letter}")
            }
            Self::NoRecommendation(id) => write!(f, "question {id} recommends nothing"),
            Self::ChangeWithoutText { question, letter } => write!(
                f,
                "option {letter} of {question} changes the plan: say how in your own words"
            ),
            Self::EndsWithGo => f.write_str(
                "the reply ends with the word go, which counts it at once: tick \"count it now\", \
                 or end it otherwise",
            ),
        }
    }
}

impl std::error::Error for ComposeError {}

/// A question's answer as the answer check reads it: its letter and the
/// words beside it; `None` for a blank answer.
fn answer_line(question: &Question, answer: &Answer) -> Result<Option<Line>, ComposeError> {
    let id = question.id.to_string();
    let text = answer.text.trim();
    let letter = match &answer.choice {
        None => None,
        Some(Choice::Letter(letter)) => {
            let Some(index) = option_index(&question.options, letter) else {
                return Err(ComposeError::NoSuchOption {
                    question: id,
                    letter: letter.clone(),
                });
            };
            Some((index, letter.clone()))
        }
        Some(Choice::Recommendation) => match &question.recommendation {
            None => return Err(ComposeError::NoRecommendation(id)),
            Some(recommendation) => match recommendation
                .option
                .as_deref()
                .and_then(|letter| option_index(&question.options, letter).map(|i| (i, letter)))
            {
                Some((index, letter)) => Some((index, letter.to_owned())),
                None => {
                    return Ok(Some(Line {
                        text: if text.is_empty() {
                            format!("{id}: ok")
                        } else {
                            format!("{id}: ok — {text}")
                        },
                        recommended: text.is_empty(),
                    }));
                }
            },
        },
    };
    let recommended_letter = question
        .recommendation
        .as_ref()
        .and_then(|r| r.option.as_deref());
    match letter {
        None if text.is_empty() => Ok(None),
        None => Ok(Some(Line {
            text: format!("{id}: {text}"),
            recommended: false,
        })),
        Some((index, letter)) => {
            if text.is_empty()
                && is_approval(question)
                && change_needs_words(&question.options[index])
            {
                return Err(ComposeError::ChangeWithoutText {
                    question: id,
                    letter,
                });
            }
            let recommended = text.is_empty() && recommended_letter == Some(letter.as_str());
            Ok(Some(Line {
                text: if text.is_empty() {
                    format!("{id}: {letter}")
                } else {
                    format!("{id}: {letter} — {text}")
                },
                recommended,
            }))
        }
    }
}

/// One line of a composed reply.
struct Line {
    text: String,
    /// The question's recommendation taken, with no words.
    recommended: bool,
}

/// The reply to `questions`, the open ask's, from the form's `answers`, in
/// the answer check's form: `ok` when every question takes its
/// recommendation with no words; otherwise one line per answered question,
/// in the ask's order, `Qn: B`, `Qn: B — <words>`, `Qn: ok` for a
/// recommendation that names no option, or `Qn: <words>`, a question left
/// blank left out. With `go`, a blank line and `go` end it, so it counts at
/// once; without, it settles through the quiet window, and a reply that
/// would end with the word go anyway is refused. An option of the plan's
/// approval that does not name its change by itself needs words
/// (`design::change_needs_words`).
pub fn compose(
    questions: &[Question],
    answers: &[Answer],
    go: bool,
) -> Result<String, ComposeError> {
    let mut seen: Vec<&str> = Vec::new();
    for answer in answers {
        if !questions.iter().any(|q| q.id.as_str() == answer.question) {
            return Err(ComposeError::UnknownQuestion(answer.question.clone()));
        }
        if seen.contains(&answer.question.as_str()) {
            return Err(ComposeError::Repeated(answer.question.clone()));
        }
        seen.push(&answer.question);
    }
    let mut lines = Vec::new();
    for question in questions {
        let Some(answer) = answers.iter().find(|a| a.question == question.id.as_str()) else {
            continue;
        };
        if let Some(line) = answer_line(question, answer)? {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        return Err(ComposeError::NoAnswer);
    }
    let body = if lines.len() == questions.len() && lines.iter().all(|line| line.recommended) {
        "ok".to_owned()
    } else {
        lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    if go {
        return Ok(format!("{body}\n\ngo"));
    }
    if ends_with_go(&body) {
        return Err(ComposeError::EndsWithGo);
    }
    Ok(body)
}

/// What the page sends: the ask it answers, by its comment, and the form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Form {
    pub ask: String,
    pub answers: Vec<Answer>,
    /// "Count it now": end the reply with `go`.
    pub go: bool,
}

/// Why a reply was not posted.
#[derive(Debug)]
pub enum ReplyError {
    /// The ticket no longer waits on the ask the page showed.
    RoundChanged,
    /// The form does not compose.
    Compose(ComposeError),
    /// The Writer did not post it: the floor refused it, or the tracker
    /// failed.
    Write(WriteError),
}

impl fmt::Display for ReplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RoundChanged => {
                f.write_str("the ticket no longer waits on the round this page shows: reload it")
            }
            Self::Compose(error) => error.fmt(f),
            Self::Write(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ReplyError {}

/// Posts the form's reply on `ticket`, whose newest record is `record`,
/// through `tracker`, the adapter of the key whose account is `key`: once
/// the record still waits on the ask the form answers, composed
/// ([`compose`]), through the UI's Writer, which asks the floor first.
pub fn send(
    tracker: &dyn Tracker,
    key: &KeyAccount,
    record: &TicketRecord,
    ticket: &TicketId,
    form: &Form,
) -> Result<Comment, ReplyError> {
    let Some(ask) = open_ask(record).filter(|ask| ask.comment == form.ask) else {
        return Err(ReplyError::RoundChanged);
    };
    let body = compose(&ask.questions, &form.answers, form.go).map_err(ReplyError::Compose)?;
    Writer::for_ui(tracker, key)
        .post_decider_reply(ticket, &body, Some(&ask.decider.account))
        .map_err(ReplyError::Write)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::RefCell;

    use owlshift_adapters::tracker::{Author, Capability, Error as TrackerError, Person, Ticket};
    use owlshift_contracts::refs::{PersistedState, TicketQuestions};
    use owlshift_contracts::review::{Fork, Plan};
    use owlshift_core::vocab::Stage;

    use super::*;
    use crate::on_demand::{PlanLink, approval_question};
    use crate::ticket_ref::fixtures;

    /// Two lettered questions, recommending A and B, as `LETTERED` asks.
    pub(crate) const LETTERED: &str = r#"[
        {"id":"Q1","category":"scope","context":"c","text":"Which language?",
         "options":[{"label":"English"},{"label":"French"}],
         "recommendation":{"option":"A","why":"English."}},
        {"id":"Q2","category":"scope","context":"c","text":"A sign-off?",
         "options":[{"label":"Yes"},{"label":"No"}],
         "recommendation":{"option":"B","why":"None needed."}}]"#;

    pub(crate) fn questions(json: &str) -> Vec<Question> {
        serde_json::from_str(json).unwrap()
    }

    fn pick(question: &str, letter: &str) -> Answer {
        Answer {
            question: question.to_owned(),
            choice: Some(Choice::Letter(letter.to_owned())),
            text: String::new(),
        }
    }

    fn with(mut answer: Answer, text: &str) -> Answer {
        answer.text = text.to_owned();
        answer
    }

    fn words(question: &str, text: &str) -> Answer {
        Answer {
            question: question.to_owned(),
            choice: None,
            text: text.to_owned(),
        }
    }

    fn recommended(question: &str) -> Answer {
        Answer {
            question: question.to_owned(),
            choice: Some(Choice::Recommendation),
            text: String::new(),
        }
    }

    fn fork(choice: &str, other: &str) -> Fork {
        Fork {
            choice: choice.to_owned(),
            options: vec![choice.to_owned(), other.to_owned()],
            reason: "R".to_owned(),
        }
    }

    fn plan(forks: Vec<Fork>) -> Plan {
        let mut plan = fixtures::plan("goal", &["a"]).plan;
        plan.forks = forks;
        plan
    }

    fn ticket() -> TicketId {
        TicketId::new("OWL-1").unwrap()
    }

    #[test]
    fn every_recommendation_taken_is_ok() {
        let asked = questions(LETTERED);
        for answers in [
            vec![pick("Q1", "A"), pick("Q2", "B")],
            vec![recommended("Q1"), recommended("Q2")],
            vec![recommended("Q1"), pick("Q2", "B")],
        ] {
            assert_eq!(compose(&asked, &answers, true).unwrap(), "ok\n\ngo");
            assert_eq!(compose(&asked, &answers, false).unwrap(), "ok");
        }
        // One question left blank is no `ok`: the other is still open.
        assert_eq!(compose(&asked, &[pick("Q1", "A")], false).unwrap(), "Q1: A");
    }

    #[test]
    fn answers_are_lines_in_ask_order() {
        let asked = questions(LETTERED);
        let answers = [with(pick("Q2", "A"), "only on main"), pick("Q1", "B")];
        assert_eq!(
            compose(&asked, &answers, false).unwrap(),
            "Q1: B\nQ2: A — only on main"
        );
        assert_eq!(
            compose(
                &asked,
                &[words("Q2", "  a short one  "), pick("Q1", "A")],
                false
            )
            .unwrap(),
            "Q1: A\nQ2: a short one"
        );
        // A recommendation with words is a line of its own.
        assert_eq!(
            compose(
                &asked,
                &[pick("Q1", "A"), with(pick("Q2", "B"), "and why")],
                false
            )
            .unwrap(),
            "Q1: A\nQ2: B — and why"
        );
        // A re-ask's question keeps its own id.
        let reasked: Vec<Question> = questions(LETTERED).into_iter().skip(1).collect();
        assert_eq!(
            compose(&reasked, &[pick("Q2", "A")], false).unwrap(),
            "Q2: A"
        );
    }

    #[test]
    fn a_recommendation_without_a_letter_is_qn_ok() {
        let asked = questions(
            r#"[{"id":"Q1","category":"scope","context":"c","text":"What should it say?",
                "recommendation":{"why":"The simplest reading."}},
               {"id":"Q2","category":"scope","context":"c","text":"Which?",
                "options":[{"label":"X"},{"label":"Y"}],
                "recommendation":{"option":"A","why":"X."}}]"#,
        );
        assert_eq!(
            compose(&asked, &[recommended("Q1"), recommended("Q2")], false).unwrap(),
            "ok"
        );
        assert_eq!(
            compose(&asked, &[recommended("Q1"), pick("Q2", "B")], false).unwrap(),
            "Q1: ok\nQ2: B"
        );
        assert_eq!(
            compose(&asked, &[with(recommended("Q1"), "but shorter")], false).unwrap(),
            "Q1: ok — but shorter"
        );
        let none = questions(
            r#"[{"id":"Q1","category":"scope","context":"c","text":"What should it say?"}]"#,
        );
        assert_eq!(
            compose(&none, &[recommended("Q1")], false),
            Err(ComposeError::NoRecommendation("Q1".into()))
        );
    }

    #[test]
    fn go_is_the_last_line_unless_unticked() {
        let asked = questions(LETTERED);
        let answers = [pick("Q1", "B"), with(pick("Q2", "A"), "Best regards")];
        let counted = compose(&asked, &answers, true).unwrap();
        assert_eq!(counted, "Q1: B\nQ2: A — Best regards\n\ngo");
        assert!(ends_with_go(&counted));
        let settles = compose(&asked, &answers, false).unwrap();
        assert!(!settles.contains("go"), "{settles}");
        assert!(!ends_with_go(&settles));
        // Unticked, a reply that ends with the word go anyway would count at
        // once: it is refused, never posted to count unasked.
        for text in ["let's go", "Go.", "and `go`!"] {
            assert_eq!(
                compose(&asked, &[with(pick("Q1", "B"), text)], false),
                Err(ComposeError::EndsWithGo),
                "{text}"
            );
            assert!(compose(&asked, &[with(pick("Q1", "B"), text)], true).is_ok());
        }
        assert_eq!(
            compose(&asked, &[with(pick("Q1", "B"), "go on")], false).unwrap(),
            "Q1: B — go on"
        );
    }

    #[test]
    fn an_empty_or_unknown_answer_is_refused() {
        let asked = questions(LETTERED);
        assert_eq!(compose(&asked, &[], true), Err(ComposeError::NoAnswer));
        assert_eq!(
            compose(&asked, &[words("Q1", "   ")], true),
            Err(ComposeError::NoAnswer)
        );
        assert_eq!(
            compose(&asked, &[pick("Q3", "A")], true),
            Err(ComposeError::UnknownQuestion("Q3".into()))
        );
        assert_eq!(
            compose(&asked, &[pick("Q1", "A"), pick("Q1", "B")], true),
            Err(ComposeError::Repeated("Q1".into()))
        );
        assert_eq!(
            compose(&asked, &[pick("Q1", "C")], true),
            Err(ComposeError::NoSuchOption {
                question: "Q1".into(),
                letter: "C".into()
            })
        );
    }

    #[test]
    fn a_plan_approval_with_its_document_needs_the_change_in_words() {
        let ticket = ticket();
        let link = PlanLink {
            ticket: &ticket,
            url: Some("https://linear.app/owlshift/document/plan-owl-1"),
        };
        let asked = vec![approval_question(
            &plan(vec![fork("Markdown", "Plain text")]),
            Some(link),
        )];
        assert_eq!(
            compose(&asked, &[pick("Q1", "A")], true).unwrap(),
            "ok\n\ngo"
        );
        assert_eq!(
            compose(
                &asked,
                &[with(pick("Q1", "B"), "pick fork 1 option B")],
                true
            )
            .unwrap(),
            "Q1: B — pick fork 1 option B\n\ngo"
        );
        assert_eq!(
            compose(&asked, &[pick("Q1", "B")], true),
            Err(ComposeError::ChangeWithoutText {
                question: "Q1".into(),
                letter: "B".into()
            })
        );
    }

    #[test]
    fn a_fork_choice_needs_no_words() {
        let asked = vec![approval_question(
            &plan(vec![
                fork("Markdown", "Plain text"),
                fork("One file", "Two"),
            ]),
            None,
        )];
        assert_eq!(asked[0].options.len(), 3);
        assert_eq!(
            compose(&asked, &[pick("Q1", "B")], true).unwrap(),
            "Q1: B\n\ngo"
        );
        assert_eq!(
            compose(&asked, &[with(pick("Q1", "C"), "and keep the flag")], true).unwrap(),
            "Q1: C — and keep the flag\n\ngo"
        );
    }

    #[test]
    fn a_plan_without_forks_is_changed_in_words() {
        let asked = vec![approval_question(&plan(Vec::new()), None)];
        assert_eq!(asked[0].options.len(), 1);
        assert_eq!(compose(&asked, &[pick("Q1", "A")], false).unwrap(), "ok");
        assert_eq!(
            compose(&asked, &[words("Q1", "Keep step 1 apart")], true).unwrap(),
            "Q1: Keep step 1 apart\n\ngo"
        );
        assert_eq!(
            compose(&asked, &[pick("Q1", "B")], true),
            Err(ComposeError::NoSuchOption {
                question: "Q1".into(),
                letter: "B".into()
            })
        );
    }

    /// A record of a ticket waiting on a round of `LETTERED` for
    /// `maintainer`, its ask posted as comment `ask-1`.
    pub(crate) fn waiting() -> TicketRecord {
        let mut record = fixtures::record(Stage::Build);
        record.state = PersistedState::parse(
            r#"{"format":4,"stage":"build","waiting":"needs_input","round":1,
            "reasks":0,"failed_runs":0,"fix_passes":0,"repairs":0,"review_passes":0}"#,
        )
        .unwrap();
        record.questions = TicketQuestions::parse(&format!(
            r#"{{"format":18,"asks":[{{"kind":"questions","round":1,
            "at":"2026-11-05T09:00:00Z","comment":"ask-1","questions":{LETTERED},
            "decider":{{"account":"maintainer","by":"assignee"}}}}],"decisions":[]}}"#
        ))
        .unwrap();
        record
    }

    /// The tracker of a key: records what it posts.
    #[derive(Default)]
    struct Posting {
        posted: RefCell<Vec<String>>,
    }

    impl Tracker for Posting {
        fn capabilities(&self) -> &'static [Capability] {
            &[Capability::Comments]
        }
        fn ticket(&self, _: &TicketId) -> Result<Ticket, TrackerError> {
            unreachable!()
        }
        fn comments(&self, _: &TicketId) -> Result<Vec<Comment>, TrackerError> {
            unreachable!()
        }
        fn post_comment(&self, _: &TicketId, body: &str) -> Result<Comment, TrackerError> {
            self.posted.borrow_mut().push(body.to_owned());
            Ok(Comment {
                id: "c1".into(),
                author: Author::Account(Person {
                    id: "maintainer".into(),
                    name: "maintainer".into(),
                }),
                created_at: "2026-11-05T10:00:00Z".parse().unwrap(),
                edited_at: None,
                body: body.to_owned(),
                parent: None,
                reactions: Vec::new(),
            })
        }
        fn set_stage(&self, _: &TicketId, _: &str) -> Result<(), TrackerError> {
            unreachable!()
        }
    }

    pub(crate) fn account(id: &str) -> KeyAccount {
        KeyAccount {
            id: id.to_owned(),
            name: "Ada".to_owned(),
            app: false,
            workspace: "owlshift".to_owned(),
        }
    }

    fn form(ask: &str) -> Form {
        Form {
            ask: ask.to_owned(),
            answers: vec![pick("Q1", "B"), recommended("Q2")],
            go: true,
        }
    }

    #[test]
    fn send_posts_the_composed_reply_through_the_ui_writer() {
        let tracker = Posting::default();
        let posted = send(
            &tracker,
            &account("maintainer"),
            &waiting(),
            &ticket(),
            &form("ask-1"),
        )
        .unwrap();
        assert_eq!(posted.body, "Q1: B\nQ2: B\n\ngo");
        assert_eq!(*tracker.posted.borrow(), ["Q1: B\nQ2: B\n\ngo"]);
        // Another account's key: the floor refuses it, nothing is posted.
        let refused = send(
            &tracker,
            &account("someone-else"),
            &waiting(),
            &ticket(),
            &form("ask-1"),
        );
        assert!(
            matches!(
                refused,
                Err(ReplyError::Write(WriteError::Floor(
                    owlshift_core::floor::FloorViolation::Reply(
                        owlshift_core::floor::ReplyRefusal::NotTheDecider
                    )
                )))
            ),
            "{refused:?}"
        );
        assert_eq!(tracker.posted.borrow().len(), 1);
    }

    #[test]
    fn send_refuses_a_round_that_changed() {
        let tracker = Posting::default();
        let key = account("maintainer");
        let other_ask = send(&tracker, &key, &waiting(), &ticket(), &form("ask-0"));
        assert!(
            matches!(other_ask, Err(ReplyError::RoundChanged)),
            "{other_ask:?}"
        );
        let mut resumed = waiting();
        resumed.state.waiting = None;
        let gone = send(&tracker, &key, &resumed, &ticket(), &form("ask-1"));
        assert!(matches!(gone, Err(ReplyError::RoundChanged)), "{gone:?}");
        let mut parked = waiting();
        parked.state.waiting = Some(Waiting::ParkedAwaitingInput);
        let parked = send(&tracker, &key, &parked, &ticket(), &form("ask-1"));
        assert!(
            matches!(parked, Err(ReplyError::RoundChanged)),
            "{parked:?}"
        );
        assert!(tracker.posted.borrow().is_empty());
    }

    pub(crate) fn ready(id: &str) -> KeyState {
        KeyState::Ready(Key {
            account: account(id),
            tracker: Box::new(
                owlshift_adapters::tracker::linear::LinearTracker::with_transport(Unreachable),
            ),
            names: Box::new(|_: &str| Ok(None)),
        })
    }

    /// A transport that answers nothing: a key's tracker no test reaches.
    struct Unreachable;

    impl owlshift_adapters::graphql::Transport for Unreachable {
        fn send(&self, _: &Value) -> Result<owlshift_adapters::graphql::Response, String> {
            Err("unreachable".into())
        }
    }

    #[test]
    fn the_form_is_offered_to_the_asks_decider_only() {
        let record = waiting();
        let offered = section(&record, &ready("maintainer"), &ticket(), &|_| None);
        assert_eq!(offered["offered"], true);
        assert!(offered["why_not"].is_null());
        assert_eq!(offered["ask"], "ask-1");
        assert_eq!(offered["round"], 1);
        assert_eq!(offered["approval"], false);
        assert_eq!(offered["words"], json!({ "Q1": [], "Q2": [] }));
        assert_eq!(offered["key"]["name"], "Ada");
        assert_eq!(offered["link"], "https://linear.app/owlshift/issue/OWL-1");
        let other = section(&record, &ready("someone-else"), &ticket(), &|_| None);
        assert_eq!(other["offered"], false);
        assert!(
            other["why_not"]
                .as_str()
                .unwrap()
                .contains("not the decider's")
        );
        assert_eq!(
            other["decider"],
            json!({ "account": "maintainer", "by": "assignee", "name": null })
        );
        assert_eq!(other["link"], "https://linear.app/owlshift/issue/OWL-1");
        let mut app = account("maintainer");
        app.app = true;
        let app = KeyState::Ready(Key {
            account: app,
            tracker: Box::new(
                owlshift_adapters::tracker::linear::LinearTracker::with_transport(Unreachable),
            ),
            names: Box::new(|_: &str| Ok(None)),
        });
        assert_eq!(
            section(&record, &app, &ticket(), &|_| None)["offered"],
            false
        );
        // A workspace key that is not plain gives no link.
        let mut odd = account("maintainer");
        odd.workspace = "x/../y".to_owned();
        let odd = KeyState::Ready(Key {
            account: odd,
            tracker: Box::new(
                owlshift_adapters::tracker::linear::LinearTracker::with_transport(Unreachable),
            ),
            names: Box::new(|_: &str| Ok(None)),
        });
        assert!(section(&record, &odd, &ticket(), &|_| None)["link"].is_null());
        // Without a read key, no form and no link: the workspace is the
        // key's to tell.
        for state in [
            KeyState::Reading,
            KeyState::Unavailable("no Linear API key is stored".into()),
        ] {
            let none = section(&record, &state, &ticket(), &|_| {
                panic!("a key not read asks for no name")
            });
            if matches!(state, KeyState::Unavailable(_)) {
                assert_eq!(
                    none["why_not"],
                    "the project's tracker key cannot be used: no Linear API key is stored. \
                     The UI reads it once: restart the UI to read it again"
                );
            }
            assert_eq!(none["offered"], false, "{state:?}");
            assert!(none["link"].is_null(), "{state:?}");
            assert!(none["why_not"].is_string(), "{state:?}");
            assert_eq!(none["decider"]["account"], "maintainer");
        }
        // A ticket that waits for no answer has no reply section.
        let mut resumed = waiting();
        resumed.state.waiting = None;
        assert!(section(&resumed, &ready("maintainer"), &ticket(), &|_| None).is_null());
    }

    /// A GitHub Issues key (OWL-552) has no workspace: the form is offered to
    /// the decider by the numeric id, and the page links no ticket, whoever
    /// the key is.
    #[test]
    fn a_github_key_is_offered_to_the_decider_with_no_link() {
        let github = |id: &str| {
            KeyState::Ready(Key {
                account: KeyAccount {
                    id: id.to_owned(),
                    name: "bea".to_owned(),
                    app: false,
                    workspace: String::new(),
                },
                tracker: Box::new(LinearTracker::with_transport(Unreachable)),
                names: Box::new(|_: &str| Ok(None)),
            })
        };
        let record = waiting();
        let offered = section(&record, &github("maintainer"), &ticket(), &|_| None);
        assert_eq!(offered["offered"], true);
        assert!(offered["link"].is_null());
        assert_eq!(offered["key"]["name"], "bea");
        let other = section(&record, &github("7"), &ticket(), &|_| None);
        assert_eq!(other["offered"], false);
        assert!(
            other["why_not"]
                .as_str()
                .unwrap()
                .contains("not the decider's")
        );
        assert!(other["link"].is_null());
    }

    /// The decider is named by the key's name when the key is theirs, else
    /// by the name read of their account so far; asked of no other account.
    #[test]
    fn the_decider_is_named_once_read() {
        let record = waiting();
        let asked = RefCell::new(Vec::new());
        let names = |account: &str| {
            asked.borrow_mut().push(account.to_owned());
            Some("person-1".to_owned())
        };
        let other = section(&record, &ready("someone-else"), &ticket(), &names);
        assert_eq!(other["decider"]["name"], "person-1");
        assert_eq!(other["decider"]["account"], "maintainer");
        assert_eq!(*asked.borrow(), ["maintainer"]);
        let not_yet = section(&record, &ready("someone-else"), &ticket(), &|_| None);
        assert!(not_yet["decider"]["name"].is_null());
        let theirs = section(&record, &ready("maintainer"), &ticket(), &|_| {
            panic!("the key's own account is not read")
        });
        assert_eq!(theirs["decider"]["name"], "Ada");
    }

    /// Keys whose one project's key is `key`, its names read by `reader`,
    /// each read counted.
    fn named_keys(
        reader: impl Fn(&str) -> Result<Option<String>, String> + Send + Sync + 'static,
    ) -> (
        Arc<Keys>,
        Arc<KeyState>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = reads.clone();
        let key = Arc::new(KeyState::Ready(Key {
            account: account("operator"),
            tracker: Box::new(LinearTracker::with_transport(Unreachable)),
            names: Box::new(move |id: &str| {
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                reader(id)
            }),
        }));
        let keys = Keys::new(Arc::new(|_: &Path, _: &Repo| Err("unused".into())));
        (keys, key, reads)
    }

    /// Waits until no name of `keys` is being read.
    fn settle(keys: &Keys) {
        while keys
            .names()
            .values()
            .any(|state| matches!(state, NameState::Reading))
        {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn a_deciders_name_is_read_once_off_the_page() {
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate_rx);
        let (keys, key, reads) = named_keys(move |id| {
            gate.lock().unwrap().recv().unwrap();
            Ok((id == "maintainer").then(|| "person-1".to_owned()))
        });
        let repo = Repo::parse("o/r").unwrap();
        // The first need starts the read and does not wait for it; the next
        // ones, while it reads, start none.
        assert_eq!(keys.decider_name(&repo, &key, "maintainer"), None);
        assert_eq!(keys.decider_name(&repo, &key, "maintainer"), None);
        gate_tx.send(()).unwrap();
        settle(&keys);
        assert_eq!(
            keys.decider_name(&repo, &key, "maintainer").as_deref(),
            Some("person-1")
        );
        // An account the tracker does not know is kept as none.
        assert_eq!(keys.decider_name(&repo, &key, "unknown"), None);
        gate_tx.send(()).unwrap();
        settle(&keys);
        assert_eq!(keys.decider_name(&repo, &key, "unknown"), None);
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
        // The key's own account (compared exactly, as `same_account` does), a
        // blank one, and a key not read: no read is even started, so the
        // names hold the two accounts read above and nothing else.
        assert_eq!(keys.decider_name(&repo, &key, "operator"), None);
        assert_eq!(keys.decider_name(&repo, &key, "  "), None);
        for state in [KeyState::Reading, KeyState::Unavailable("none".into())] {
            assert_eq!(keys.decider_name(&repo, &Arc::new(state), "other"), None);
        }
        assert_eq!(keys.names().len(), 2);
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failed_name_is_read_again_after_a_while() {
        let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let fails = failing.clone();
        let (keys, key, reads) = named_keys(move |_| {
            if fails.load(std::sync::atomic::Ordering::SeqCst) {
                Err("Linear: unreachable".into())
            } else {
                Ok(Some("person-1".into()))
            }
        });
        let repo = Repo::parse("o/r").unwrap();
        let start = Instant::now();
        assert_eq!(keys.decider_name_at(&repo, &key, "maintainer", start), None);
        settle(&keys);
        failing.store(false, std::sync::atomic::Ordering::SeqCst);
        // Within the while, the failure is kept and nothing is read.
        assert_eq!(keys.decider_name_at(&repo, &key, "maintainer", start), None);
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Past it, the next need reads it again, once.
        let later = start + NAME_RETRY + std::time::Duration::from_secs(1);
        assert_eq!(keys.decider_name_at(&repo, &key, "maintainer", later), None);
        settle(&keys);
        assert_eq!(
            keys.decider_name_at(&repo, &key, "maintainer", later)
                .as_deref(),
            Some("person-1")
        );
        assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn a_read_that_panics_is_a_failure() {
        let (keys, key, _) = named_keys(|_| panic!("the reader broke"));
        let repo = Repo::parse("o/r").unwrap();
        assert_eq!(keys.decider_name(&repo, &key, "maintainer"), None);
        settle(&keys);
        assert!(matches!(
            keys.names().values().next(),
            Some(NameState::Failed(_))
        ));
    }

    #[test]
    fn the_words_an_approval_needs_are_listed_by_letter() {
        let ticket = ticket();
        let link = PlanLink {
            ticket: &ticket,
            url: None,
        };
        let mut record = waiting();
        let question = approval_question(&plan(vec![fork("Markdown", "Plain text")]), Some(link));
        record.questions.asks[0].questions = vec![question];
        let reply = section(&record, &ready("maintainer"), &ticket, &|_| None);
        assert_eq!(reply["approval"], true);
        assert_eq!(reply["words"], json!({ "Q1": ["B"] }));
        record.questions.asks[0].questions = vec![approval_question(
            &plan(vec![fork("Markdown", "Plain text")]),
            None,
        )];
        let reply = section(&record, &ready("maintainer"), &ticket, &|_| None);
        assert_eq!(reply["words"], json!({ "Q1": [] }));
    }

    #[test]
    fn a_markdown_project_has_no_key_and_the_keychain_is_not_asked() {
        let data = tempfile::tempdir().unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let dirs = ProjectDirs::new(data.path(), &repo);
        std::fs::create_dir_all(dirs.checkout()).unwrap();
        let none = open_key(data.path(), &repo).unwrap_err();
        assert!(none.contains("no project file"), "{none}");
        let mut record = owlshift_contracts::service::ServedConfig::new("o/r");
        record.kept = Some(owlshift_contracts::service::KeptFile {
            branch: "main".to_owned(),
            commit: "a".repeat(40),
            text: "requires = \">=0.1\"\n[tracker]\nkind = \"markdown\"\n\
                   admit = { label = \"owlshift\" }\n\
                   states = { ready = \"Todo\", working = \"Doing\", needs_input = \"Asked\", \
                   review = \"Review\" }\n[stack]\ngate = [\"make test\"]\n[pipeline]\n\
                   default = \"trivial\"\nplan_approval = \"never\"\n[models]\n[policy]\n\
                   always_human = []\n"
                .to_owned(),
            since: "2026-11-01T00:00:00Z".parse().unwrap(),
        });
        std::fs::create_dir_all(crate::served::record_file(&dirs).parent().unwrap()).unwrap();
        std::fs::write(crate::served::record_file(&dirs), record.render()).unwrap();
        let markdown = open_key(data.path(), &repo).unwrap_err();
        assert!(markdown.contains("Markdown tracker"), "{markdown}");
    }

    /// The keys are read once each, whatever asks; a failure is kept.
    #[test]
    fn a_key_is_read_once_and_kept() {
        let opened = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = opened.clone();
        let keys = Keys::new(Arc::new(move |_: &Path, repo: &Repo| {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(format!("no key for {repo}"))
        }));
        let data = tempfile::tempdir().unwrap();
        let (one, two) = (Repo::parse("o/one").unwrap(), Repo::parse("o/two").unwrap());
        keys.read_all(data.path(), vec![one.clone()]);
        let settled = |repo: &Repo| loop {
            match &*keys.state(data.path(), repo) {
                KeyState::Reading => std::thread::sleep(std::time::Duration::from_millis(5)),
                KeyState::Unavailable(why) => return why.clone(),
                KeyState::Ready(_) => panic!("ready"),
            }
        };
        assert_eq!(settled(&one), "no key for o/one");
        assert_eq!(settled(&two), "no key for o/two");
        for _ in 0..3 {
            settled(&one);
            settled(&two);
        }
        keys.read_all(data.path(), vec![one, two]);
        assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
