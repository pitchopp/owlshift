//! The Writer's refresh of the GitHub App's token (OWL-384, build plan GA3;
//! architecture section 8, "The GitHub App's tokens").
//!
//! The app's access token lives 8 hours and its refresh token 182 days;
//! a refresh rotates both and revokes the pair it replaces (C13,
//! 2026-10-19). [`AppToken`] is the one place that refreshes: the forge's
//! requests (`do`, `continue`, watch) and `owlshift init`'s check of a
//! stored token all go through it.
//!
//! - A token past its stored expiry is refreshed before it is sent; a
//!   request GitHub answers HTTP 401 while a refresh token is stored gets
//!   a refresh and is made once more ([`TokenSource`]).
//! - The refresh is serialised across processes by the account lock, the
//!   file [`ProjectDirs::account_lock_file`](crate::project::ProjectDirs::account_lock_file)
//!   beside the project's `lock`, waited for at most [`ACCOUNT_LOCK_WAIT`].
//!   It is held for the refresh and its keychain write alone; nothing here
//!   takes or waits for the project lock, so the account lock is never held
//!   while the project lock is waited for.
//! - Under the lock the keychain is read again: a pair another process
//!   stored meanwhile is used as it is, and the refresh is sent only while
//!   the stored access token is still the one found expired or refused.
//! - The new pair is written to the keychain before its access token is
//!   sent; a write that fails [`STORE_TRIES`] times loses the rotation.
//! - A refresh GitHub refuses, or a rotation lost, is
//!   [`ErrorKind::SignInAgain`], naming `owlshift init`; a refresh token
//!   spent or refused is never sent again by this source. No personal token
//!   is ever tried in its place.
//! - Each refresh and each keychain write of the pair is an event of the
//!   data directory's log ([`TokenEvents`], OWL-649): `sending` before the
//!   request and its outcome, or why none was sent, so a rotation lost can
//!   be traced (`docs/design/records/OWL-649.md`).
//! - No refresh is sent while macOS is in a dark wake (OWL-649, OWL-485's
//!   probe): a dark wake sleeps again within seconds, and an answer lost
//!   after GitHub rotated spends the refresh token, which the next refresh
//!   then sends and GitHub refuses. The call fails as one GitHub did not
//!   answer ([`ErrorKind::Unreachable`]), the refresh token unspent, and
//!   the first call in a full wake refreshes. A reading the probe cannot
//!   make holds nothing.
//!
//! No token reaches an error, an event or `Debug` output.

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use jiff::Timestamp;
use owlshift_adapters::forge::github::{Token, TokenSource};
use owlshift_adapters::forge::github_app::{AppError, DeviceFlow, Login, UserToken};
use owlshift_adapters::forge::{Error, ErrorKind};
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::format::Format;
use owlshift_platform::keychain::{Keychain, Secret};
use owlshift_platform::wake::{self, Wake};
use serde_json::json;

use crate::config::OWLSHIFT_VERSION;
use crate::events::{self, Data, EventLog};
use crate::forge::sign_in_again;
use crate::project::lock_file_within;

/// The longest wait for the account lock: past a holder's whole refresh,
/// whose request times out after 30 s, and its keychain write, and far past
/// the moments a child keeps a lock just released
/// ([`ProjectDirs::lock`](crate::project::ProjectDirs::lock)).
pub const ACCOUNT_LOCK_WAIT: Duration = Duration::from_secs(60);

/// How often the account lock is tried while it is waited for.
pub const ACCOUNT_LOCK_POLL: Duration = Duration::from_millis(50);

/// How many times a rotated pair's keychain write is tried before the
/// rotation counts as lost: GitHub already revoked the pair it replaced.
pub const STORE_TRIES: u32 = 3;

/// The pause between two tries of that write.
const STORE_PAUSE: Duration = Duration::from_millis(100);

/// The keychain entries the refresh reads and writes: the system keychain,
/// or a fake in tests. Errors are in Owlshift's words.
pub trait Entries {
    fn read(&self, account: &str) -> Result<Option<Secret>, String>;
    fn store(&self, account: &str, secret: &Secret) -> Result<(), String>;
}

impl Entries for Keychain {
    fn read(&self, account: &str) -> Result<Option<Secret>, String> {
        Keychain::read(self, account).map_err(|error| error.to_string())
    }

    fn store(&self, account: &str, secret: &Secret) -> Result<(), String> {
        Keychain::store(self, account, secret).map_err(|error| error.to_string())
    }
}

impl<E: Entries + ?Sized> Entries for &E {
    fn read(&self, account: &str) -> Result<Option<Secret>, String> {
        (**self).read(account)
    }

    fn store(&self, account: &str, secret: &Secret) -> Result<(), String> {
        (**self).store(account, secret)
    }
}

impl<E: Entries + ?Sized> Entries for Arc<E> {
    fn read(&self, account: &str) -> Result<Option<Secret>, String> {
        (**self).read(account)
    }

    fn store(&self, account: &str, secret: &Secret) -> Result<(), String> {
        (**self).store(account, secret)
    }
}

/// The refresh grant: the app's [`DeviceFlow`], or a borrow of one.
pub trait Refresh {
    fn refresh(&self, refresh_token: &str) -> Result<UserToken, AppError>;
}

impl<L: Login> Refresh for DeviceFlow<L> {
    fn refresh(&self, refresh_token: &str) -> Result<UserToken, AppError> {
        DeviceFlow::refresh(self, refresh_token)
    }
}

impl<R: Refresh + ?Sized> Refresh for &R {
    fn refresh(&self, refresh_token: &str) -> Result<UserToken, AppError> {
        (**self).refresh(refresh_token)
    }
}

/// The events of one project's GitHub App token (OWL-649, architecture
/// section 8, "Its events"), appended to a data directory's log, which
/// `owlshift logs` prints: each refresh, and each keychain write or removal
/// of the pair. They are project events, of no ticket and no run, keyed
/// `github_app` (`refresh` or `keychain_write`): a `decision` when it went as
/// meant, a `warning` saying `why` otherwise. Each holds its `outcome` and
/// the process (`pid`, `command`, `version`), with expiries in RFC 3339;
/// never a token. A failed append is said on standard error and never fails
/// what it records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenEvents {
    log: EventLog,
    /// The project's name, `owner/repo`, as `owlshift do`'s events name it.
    project: String,
}

/// How a keychain write or removal of the GitHub App's pair ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Written {
    /// The pair was stored.
    Stored,
    /// The pair was removed.
    Removed,
    /// No pair was stored to remove.
    NoneStored,
    /// The write or the removal failed, for this reason in Owlshift's
    /// words, which hold no stored value.
    Failed(String),
}

impl TokenEvents {
    /// The events of `project`'s token, in `log`.
    pub fn new(log: EventLog, project: impl Into<String>) -> Self {
        Self {
            log,
            project: project.into(),
        }
    }

    /// The log they are appended to.
    pub fn log(&self) -> &EventLog {
        &self.log
    }

    /// Records a keychain write or removal of the pair, `written`, by `by`
    /// (`refresh`, `sign_in`, `removal` or `uninstall`), with the expiries
    /// of `pair` when one was written and the details `more`.
    pub fn keychain_write(
        &self,
        by: &str,
        written: &Written,
        pair: Option<&UserToken>,
        mut more: Data,
    ) {
        more.insert("by".to_owned(), json!(by));
        if let Some(pair) = pair {
            more.extend(expiries(pair));
        }
        let (outcome, why) = match written {
            Written::Stored => ("stored", None),
            Written::Removed => ("removed", None),
            Written::NoneStored => ("none_stored", None),
            Written::Failed(why) => ("failed", Some(why.as_str())),
        };
        self.record("keychain_write", outcome, why, more);
    }

    /// Records a refresh's `outcome`.
    fn refresh(&self, outcome: &str, why: Option<&str>, more: Data) {
        self.record("refresh", outcome, why, more);
    }

    /// Appends one event of `what`, `github_app`, with `outcome`, the
    /// process and `data`: a `decision`, or a `warning` when there is `why`.
    fn record(&self, what: &str, outcome: &str, why: Option<&str>, mut data: Data) {
        data.insert("github_app".to_owned(), json!(what));
        data.insert("outcome".to_owned(), json!(outcome));
        data.insert("pid".to_owned(), json!(std::process::id()));
        data.insert(
            "command".to_owned(),
            json!(command_name(std::env::args_os().nth(1))),
        );
        data.insert("version".to_owned(), json!(OWLSHIFT_VERSION));
        let kind = match why {
            None => EventKind::Decision,
            Some(why) => {
                data.insert("what".to_owned(), json!("github_app_token"));
                data.insert("why".to_owned(), json!(why));
                EventKind::Warning
            }
        };
        let event = Event {
            format: Format,
            at: Timestamp::now(),
            project: self.project.clone(),
            ticket: None,
            run: None,
            kind,
            data,
        };
        if let Err(error) = self.log.append(&event) {
            eprintln!(
                "{}",
                events::printable(&format!(
                    "owlshift: warning: an event of the GitHub App's token was not recorded in \
                     {}: {error}",
                    self.log.path().display()
                ))
            );
        }
    }
}

/// The expiries of `pair`, as an event holds them.
fn expiries(pair: &UserToken) -> Data {
    let mut data = Data::new();
    if let Some(at) = pair.expires_at() {
        data.insert("expires_at".to_owned(), json!(at.to_string()));
    }
    if let Some(at) = pair.refresh_expires_at() {
        data.insert("refresh_expires_at".to_owned(), json!(at.to_string()));
    }
    data
}

/// The process's command as an event names it: `first`, its first
/// argument, when it is a lowercase word of letters and `-` of at most 24
/// characters, such as `watch` or `continue`; `owlshift` otherwise.
fn command_name(first: Option<OsString>) -> String {
    first
        .and_then(|first| first.into_string().ok())
        .filter(|first| {
            first.len() <= 24
                && first.starts_with(|c: char| c.is_ascii_lowercase())
                && first.chars().all(|c| c.is_ascii_lowercase() || c == '-')
        })
        .unwrap_or_else(|| "owlshift".to_owned())
}

/// What a refresh was asked on: a token past its stored expiry, or one
/// GitHub answered HTTP 401.
#[derive(Clone, Copy)]
enum Trigger {
    Expired,
    Http401,
}

impl Trigger {
    fn name(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Http401 => "http_401",
        }
    }
}

/// The GitHub App's token of one project, as the runner holds it: the pair
/// last read or written, refreshed through `flow` under the account lock.
pub struct AppToken<E: Entries, R: Refresh> {
    entries: E,
    /// The project's keychain account, `github.com/<owner>/<repo>/github-app`.
    account: String,
    /// The account lock's file.
    lock: PathBuf,
    flow: R,
    held: UserToken,
    /// The refresh token last sent or refused: never sent again, so a
    /// refused refresh is not repeated on every request of a watch.
    dead: Option<String>,
    lock_wait: Duration,
    now: fn() -> Timestamp,
    /// Where each refresh and write is recorded, when anywhere.
    events: Option<TokenEvents>,
    /// The reason of the last `not_sent` event recorded: not recorded again
    /// while it repeats, so a watch whose token died adds one event, not one
    /// per request; a token handed out or a refresh sent starts over.
    told: Option<&'static str>,
    /// The machine's power state, read just before a refresh is sent.
    wake: fn() -> Wake,
}

/// Why a refresh is held in a dark wake, as the call that needed it fails.
pub const DARK_WAKE_HOLD: &str = "the GitHub App's token was \
     not refreshed: the machine is in a dark wake (macOS keeps it up without graphics) and may \
     sleep again before GitHub answers, which would lose the rotation; it is refreshed at the \
     next full wake";

impl<E: Entries, R: Refresh> fmt::Debug for AppToken<E, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppToken")
            .field("account", &self.account)
            .field("held", &self.held)
            .finish_non_exhaustive()
    }
}

impl<E: Entries, R: Refresh> AppToken<E, R> {
    /// The token stored as `entry` under `account`, refreshed through `flow`
    /// under the lock file `lock`, on the system clock. An entry that does
    /// not read is [`ErrorKind::SignInAgain`].
    pub fn new(
        entries: E,
        account: impl Into<String>,
        lock: PathBuf,
        flow: R,
        entry: &Secret,
    ) -> Result<Self, Error> {
        let account = account.into();
        let held = UserToken::from_entry(entry.expose()).map_err(|_| {
            Error::new(
                ErrorKind::SignInAgain,
                sign_in_again(&account, "does not read"),
            )
        })?;
        Ok(Self {
            entries,
            account,
            lock,
            flow,
            held,
            dead: None,
            lock_wait: ACCOUNT_LOCK_WAIT,
            now: Timestamp::now,
            events: None,
            told: None,
            wake: wake::read,
        })
    }

    /// The same source reading the machine's power state with `wake`, which
    /// tests set so that none reads the machine it runs on.
    pub fn with_wake(self, wake: fn() -> Wake) -> Self {
        Self { wake, ..self }
    }

    /// The same source on the clock `now`, which [`TokenSource`] reads.
    pub fn with_clock(self, now: fn() -> Timestamp) -> Self {
        Self { now, ..self }
    }

    /// The same source, recording each refresh and write in `events`.
    pub fn with_events(self, events: TokenEvents) -> Self {
        Self {
            events: Some(events),
            ..self
        }
    }

    /// The token to send at `now`: the one held, refreshed first when it is
    /// past its stored expiry.
    pub fn token_at(&mut self, now: Timestamp) -> Result<Token, Error> {
        if !self.held.expired(now) {
            return Ok(Token::new(self.held.access()));
        }
        let stale = Token::new(self.held.access());
        self.renew(&stale, now, Trigger::Expired)?
            .ok_or_else(|| self.sign_in_again("has expired and holds no refresh token"))
    }

    /// GitHub answered HTTP 401 to `refused` at `now`: the token to send
    /// once more, or `None` when no refresh token is stored.
    pub fn refused_at(&mut self, refused: &Token, now: Timestamp) -> Result<Option<Token>, Error> {
        self.renew(refused, now, Trigger::Http401)
    }

    /// Under the account lock: the stored pair when another process stored
    /// one in place of `stale`, else a refresh of the stored pair, written
    /// before it is used; `None` when the stored pair has no refresh token.
    /// Each call leaves its events: `sending` and the refresh's outcome, or
    /// why none was sent. Nothing is sent in a dark wake ([`DARK_WAKE_HOLD`]).
    fn renew(
        &mut self,
        stale: &Token,
        now: Timestamp,
        trigger: Trigger,
    ) -> Result<Option<Token>, Error> {
        let _lock = match self.account_lock() {
            Ok(lock) => lock,
            Err((reason, error)) => return Err(self.not_sent(trigger, reason, None, error)),
        };
        let stored = match self.entries.read(&self.account) {
            Ok(Some(entry)) => match UserToken::from_entry(entry.expose()) {
                Ok(stored) => stored,
                Err(_) => {
                    let error = self.sign_in_again("does not read");
                    return Err(self.not_sent(trigger, "unreadable", None, error));
                }
            },
            Ok(None) => {
                let error = self.sign_in_again("is no longer stored");
                return Err(self.not_sent(trigger, "not_stored", None, error));
            }
            Err(error) => {
                let error = Error::new(
                    ErrorKind::Other,
                    format!(
                        "the GitHub App's token could not be read from the system keychain: \
                         {error}"
                    ),
                );
                return Err(self.not_sent(trigger, "keychain_unreadable", None, error));
            }
        };
        if Token::new(stored.access()) != *stale && !stored.expired(now) {
            self.refresh_event(
                "stored_pair_used",
                trigger,
                Some(&stored),
                None,
                Data::new(),
            );
            self.told = None;
            self.held = stored;
            return Ok(Some(Token::new(self.held.access())));
        }
        let Some(refresh) = stored.refresh().map(str::to_owned) else {
            self.tell(
                trigger,
                "no_refresh_token",
                Some(&stored),
                Some("the stored pair holds no refresh token"),
            );
            self.held = stored;
            return Ok(None);
        };
        if self.dead.as_deref() == Some(refresh.as_str()) {
            let error = self.sign_in_again("was refused its refresh");
            return Err(self.not_sent(trigger, "refused_before", Some(&stored), error));
        }
        if stored.refresh_expires_at().is_some_and(|at| now >= at) {
            let error = self.sign_in_again("has a refresh token past its life");
            return Err(self.not_sent(trigger, "past_its_life", Some(&stored), error));
        }
        // The last check before the request, under the lock and after the
        // keychain's read, so a pair a peer stored is still used in a dark
        // wake. A dark wake may sleep before GitHub answers, and an answer
        // lost after it rotated spends the refresh token (OWL-649's record):
        // nothing is sent, and the token is kept for the full wake.
        let wake = match (self.wake)() {
            Wake::Dark => {
                self.tell(trigger, "dark_wake", Some(&stored), None);
                return Err(Error::new(ErrorKind::Unreachable, DARK_WAKE_HOLD));
            }
            Wake::Full => "full",
            Wake::Unknown => "unknown",
        };
        self.told = None;
        let more = events::data([("wake", json!(wake))]);
        self.refresh_event("sending", trigger, Some(&stored), None, more);
        // Spent from here on: GitHub may rotate even if its answer is lost.
        self.dead = Some(refresh.clone());
        let new = match self.flow.refresh(&refresh) {
            Ok(new) => new,
            Err(refused @ AppError::Refused(_)) => {
                let error = self.sign_in_again(&format!("could not be refreshed: {refused}"));
                self.refresh_event(
                    "refused",
                    trigger,
                    Some(&stored),
                    Some(&error.message),
                    Data::new(),
                );
                return Err(error);
            }
            Err(other) => {
                // No answer, or one a retry may clear: the token is tried
                // again next time; a rotation GitHub made meanwhile shows as
                // a refused refresh then (OWL-649's record). No answer at all
                // is its own kind, which a watch starting waits on (OWL-479).
                self.dead = None;
                let (kind, outcome) = match other {
                    AppError::Unreachable(_) => (ErrorKind::Unreachable, "no_answer"),
                    _ => (ErrorKind::Other, "failed"),
                };
                let error = Error::new(
                    kind,
                    format!("the GitHub App's token could not be refreshed: {other}"),
                );
                self.refresh_event(
                    outcome,
                    trigger,
                    Some(&stored),
                    Some(&error.message),
                    Data::new(),
                );
                return Err(error);
            }
        };
        // Before the write: a `rotated` with no write after it is a process
        // that ended between GitHub's answer and the keychain.
        self.refresh_event("rotated", trigger, Some(&stored), None, expiries(&new));
        self.write(&new)?;
        self.held = new;
        Ok(Some(Token::new(self.held.access())))
    }

    /// Records a refresh event of `outcome`, with the trigger, the expiry
    /// of the pair held and of the one `stored`, once read.
    fn refresh_event(
        &self,
        outcome: &str,
        trigger: Trigger,
        stored: Option<&UserToken>,
        why: Option<&str>,
        mut more: Data,
    ) {
        let Some(events) = &self.events else {
            return;
        };
        more.insert("trigger".to_owned(), json!(trigger.name()));
        if let Some(at) = self.held.expires_at() {
            more.insert("held_expires_at".to_owned(), json!(at.to_string()));
        }
        if let Some(at) = stored.and_then(UserToken::expires_at) {
            more.insert("stored_expires_at".to_owned(), json!(at.to_string()));
        }
        events.refresh(outcome, why, more);
    }

    /// Records that no refresh was sent, for `reason`, and gives `error`
    /// back.
    fn not_sent(
        &mut self,
        trigger: Trigger,
        reason: &'static str,
        stored: Option<&UserToken>,
        error: Error,
    ) -> Error {
        self.tell(trigger, reason, stored, Some(&error.message));
        error
    }

    /// Records a `not_sent` event for `reason`, a warning saying `why` when
    /// given, unless the last one recorded gave the same reason.
    fn tell(
        &mut self,
        trigger: Trigger,
        reason: &'static str,
        stored: Option<&UserToken>,
        why: Option<&str>,
    ) {
        if self.told == Some(reason) {
            return;
        }
        self.told = Some(reason);
        let more = events::data([("reason", json!(reason))]);
        self.refresh_event("not_sent", trigger, stored, why, more);
    }

    /// Writes `new` under the account, tried [`STORE_TRIES`] times, each try
    /// recorded.
    fn write(&self, new: &UserToken) -> Result<(), Error> {
        let entry = Secret::new(new.to_entry());
        let mut tries = 0;
        loop {
            tries += 1;
            let stored = self.entries.store(&self.account, &entry);
            let last = tries >= STORE_TRIES;
            if let Some(events) = &self.events {
                let mut more = events::data([("try", json!(tries)), ("tries", json!(STORE_TRIES))]);
                let written = match &stored {
                    Ok(()) => Written::Stored,
                    Err(why) => {
                        if last {
                            more.insert("rotation_lost".to_owned(), json!(true));
                        }
                        Written::Failed(why.clone())
                    }
                };
                events.keychain_write("refresh", &written, Some(new), more);
            }
            match stored {
                Ok(()) => return Ok(()),
                Err(error) if last => {
                    return Err(self.sign_in_again(&format!(
                        "was rotated by GitHub, but storing the new one failed ({error}), so the \
                         rotation is lost"
                    )));
                }
                Err(_) => thread::sleep(STORE_PAUSE),
            }
        }
    }

    /// The account lock, waited for at most `lock_wait`; else why not, as
    /// a `not_sent` reason (`lock_failed` or `lock_held`) and an error.
    fn account_lock(&self) -> Result<crate::project::FileLock, (&'static str, Error)> {
        let failed = |error: std::io::Error| {
            (
                "lock_failed",
                Error::new(
                    ErrorKind::Other,
                    format!(
                        "the GitHub App's account lock {} could not be taken: {error}",
                        self.lock.display()
                    ),
                ),
            )
        };
        if let Some(parent) = self.lock.parent() {
            fs::create_dir_all(parent).map_err(failed)?;
        }
        lock_file_within(&self.lock, self.lock_wait, ACCOUNT_LOCK_POLL)
            .map_err(failed)?
            .ok_or_else(|| {
                (
                    "lock_held",
                    Error::new(
                        ErrorKind::Other,
                        format!(
                            "another Owlshift process held the GitHub App's account lock {} for \
                             more than {} s, refreshing its token: try again",
                            self.lock.display(),
                            self.lock_wait.as_secs()
                        ),
                    ),
                )
            })
    }

    fn sign_in_again(&self, what: &str) -> Error {
        Error::new(ErrorKind::SignInAgain, sign_in_again(&self.account, what))
    }
}

impl<E: Entries + Send, R: Refresh + Send> TokenSource for AppToken<E, R> {
    fn token(&mut self) -> Result<Token, Error> {
        self.token_at((self.now)())
    }

    fn refused(&mut self, refused: &Token) -> Result<Option<Token>, Error> {
        self.refused_at(refused, (self.now)())
    }
}

/// Fakes the tests of the refresh and of its callers share: keychain
/// entries that fail as asked, and a GitHub that rotates.
#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::Mutex;

    use owlshift_adapters::forge::github_app::LoginAnswer;
    use owlshift_contracts::event::Event;

    use super::*;

    /// How [`Faulty`] fails.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Fault {
        /// Every read fails.
        ReadFails,
        /// Every store fails.
        StoreFails,
    }

    /// Keychain entries over an in-memory keychain that fail as `fault`
    /// says, counting the stores tried.
    pub(crate) struct Faulty<'a> {
        pub(crate) keychain: &'a Keychain,
        pub(crate) fault: Fault,
        pub(crate) stores: Mutex<u32>,
    }

    impl<'a> Faulty<'a> {
        pub(crate) fn new(keychain: &'a Keychain, fault: Fault) -> Self {
            Self {
                keychain,
                fault,
                stores: Mutex::new(0),
            }
        }

        pub(crate) fn stores(&self) -> u32 {
            *self.stores.lock().unwrap()
        }
    }

    impl Entries for Faulty<'_> {
        fn read(&self, account: &str) -> Result<Option<Secret>, String> {
            match self.fault {
                Fault::ReadFails => Err("the keychain is locked".to_owned()),
                _ => Entries::read(self.keychain, account),
            }
        }

        fn store(&self, account: &str, secret: &Secret) -> Result<(), String> {
            *self.stores.lock().unwrap() += 1;
            match self.fault {
                Fault::StoreFails => Err("the keychain is locked".to_owned()),
                _ => Entries::store(self.keychain, account, secret),
            }
        }
    }

    /// What [`Rotating`] does with a refresh it accepts.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Answer {
        /// Rotates and answers the new pair.
        Given,
        /// Rotates, and its answer is lost: the caller sees no answer.
        Lost,
    }

    /// GitHub's token endpoint as C13 found it: it accepts only the refresh
    /// token it issued last, rotates it (`ghr_1`, `ghr_2`, …, with
    /// `ghu_1`, `ghu_2`, …), and answers any other with HTTP 200 and
    /// `incorrect_client_credentials`. Each accepted refresh takes the next
    /// of its answers, [`Answer::Given`] once they run out.
    pub(crate) struct Rotating {
        state: Mutex<RotatingState>,
    }

    struct RotatingState {
        current: String,
        issued: u32,
        answers: VecDeque<Answer>,
        sent: Vec<String>,
    }

    impl Rotating {
        /// GitHub holding `current` as the refresh token it issued last.
        pub(crate) fn new(current: &str, answers: &[Answer]) -> Self {
            Self {
                state: Mutex::new(RotatingState {
                    current: current.to_owned(),
                    issued: 0,
                    answers: answers.iter().copied().collect(),
                    sent: Vec::new(),
                }),
            }
        }

        /// The refresh tokens sent, in order.
        pub(crate) fn sent(&self) -> Vec<String> {
            self.state.lock().unwrap().sent.clone()
        }
    }

    impl Login for &Rotating {
        fn post_form(&self, _: &str, form: &[(&str, &str)]) -> Result<LoginAnswer, String> {
            let refresh = form
                .iter()
                .find(|(key, _)| *key == "refresh_token")
                .map(|(_, value)| (*value).to_owned())
                .expect("a refresh");
            let mut state = self.state.lock().unwrap();
            state.sent.push(refresh.clone());
            if refresh != state.current {
                return Ok(LoginAnswer::new(
                    200,
                    r#"{"error":"incorrect_client_credentials"}"#,
                ));
            }
            state.issued += 1;
            let issued = state.issued;
            state.current = format!("ghr_{issued}");
            match state.answers.pop_front().unwrap_or(Answer::Given) {
                Answer::Lost => Err("GitHub's login endpoint could not be reached".to_owned()),
                Answer::Given => Ok(LoginAnswer::new(
                    200,
                    format!(
                        r#"{{"access_token":"ghu_{issued}","expires_in":28800,"refresh_token":"ghr_{issued}","refresh_token_expires_in":15724800,"token_type":"bearer"}}"#
                    ),
                )),
            }
        }
    }

    /// The events of the log in `data`, oldest first; none when it does
    /// not exist. It panics when the log holds a token.
    pub(crate) fn logged(data: &Path) -> Vec<Event> {
        let text = fs::read_to_string(EventLog::in_dir(data).path()).unwrap_or_default();
        assert!(
            !text.contains("ghu_") && !text.contains("ghr_"),
            "a token in the log: {text}"
        );
        text.lines()
            .map(|line| Event::parse(line).unwrap())
            .collect()
    }

    /// Each token event's `github_app` and `outcome`, with its `reason` when
    /// it has one, as `refresh not_sent dark_wake`.
    pub(crate) fn told(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| {
                let text = |key: &str| event.data.get(key).and_then(|v| v.as_str());
                let mut line = format!("{} {}", text("github_app")?, text("outcome")?);
                if let Some(reason) = text("reason") {
                    line.push(' ');
                    line.push_str(reason);
                }
                Some(line)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use owlshift_adapters::forge::Repo;
    use owlshift_adapters::forge::github::{Connect, GitHubForge, Response, Transport};
    use owlshift_adapters::forge::github_app::LoginAnswer;
    use serde_json::Value;

    use super::testing::{Answer, Fault, Faulty, Rotating, logged, told};
    use super::*;

    const ACCOUNT: &str = "github.com/owner/repo/github-app";
    const HEAD: &str = "36f0557116ea73f9b0805980e83b360e23b99940";

    fn now() -> Timestamp {
        Timestamp::from_second(1_760_000_000).unwrap()
    }

    fn at(offset: i64) -> Timestamp {
        Timestamp::from_second(now().as_second() + offset).unwrap()
    }

    fn entry(access: &str, refresh: Option<&str>, expires_at: i64) -> Secret {
        Secret::new(
            UserToken::new(
                access,
                refresh.map(str::to_owned),
                Some(at(expires_at)),
                Some(at(15_724_800)),
            )
            .to_entry(),
        )
    }

    fn granted(access: &str, refresh: &str) -> String {
        format!(
            r#"{{"access_token":"{access}","expires_in":28800,"refresh_token":"{refresh}","refresh_token_expires_in":15724800,"token_type":"bearer"}}"#
        )
    }

    const REFUSED: &str = r#"{"error":"bad_refresh_token"}"#;

    /// An answer [`Script`] gives as no answer at all.
    const NO_ANSWER: &str = "<no answer>";

    /// The first refresh says it started on the sender, then waits for the
    /// receiver to release it.
    type Hold = (Mutex<mpsc::Sender<()>>, Mutex<mpsc::Receiver<()>>);

    /// GitHub's token endpoint: each refresh takes the next answer and
    /// records the refresh token sent, held first by `hold`.
    #[derive(Default)]
    struct Script {
        answers: Mutex<VecDeque<String>>,
        sent: Mutex<Vec<String>>,
        hold: Option<Hold>,
    }

    impl Script {
        fn answering(answers: &[&str]) -> Self {
            Self {
                answers: Mutex::new(answers.iter().map(|a| (*a).to_owned()).collect()),
                ..Self::default()
            }
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl Login for &Script {
        fn post_form(&self, _: &str, form: &[(&str, &str)]) -> Result<LoginAnswer, String> {
            let refresh = form
                .iter()
                .find(|(key, _)| *key == "refresh_token")
                .map(|(_, value)| (*value).to_owned())
                .expect("a refresh");
            let first = {
                let mut sent = self.sent.lock().unwrap();
                sent.push(refresh);
                sent.len() == 1
            };
            if let (true, Some((started, release))) = (first, &self.hold) {
                started.lock().unwrap().send(()).unwrap();
                release.lock().unwrap().recv().unwrap();
            }
            let body = self.answers.lock().unwrap().pop_front().expect("an answer");
            if body == NO_ANSWER {
                return Err("GitHub's login endpoint could not be reached".to_owned());
            }
            Ok(LoginAnswer::new(200, body))
        }
    }

    fn source<'a>(
        keychain: &'a Keychain,
        script: &'a Script,
        lock: PathBuf,
    ) -> AppToken<&'a Keychain, DeviceFlow<&'a Script>> {
        let stored = keychain.read(ACCOUNT).unwrap().expect("a stored token");
        AppToken::new(
            keychain,
            ACCOUNT,
            lock,
            DeviceFlow::new(script, "Iv1.test").with_clock(now),
            &stored,
        )
        .unwrap()
        .with_wake(|| Wake::Full)
    }

    fn stored_access(keychain: &Keychain) -> String {
        let entry = keychain.read(ACCOUNT).unwrap().unwrap();
        UserToken::from_entry(entry.expose())
            .unwrap()
            .access()
            .to_owned()
    }

    /// GitHub's GraphQL API for one token: `ok` answers the default branch,
    /// any other token 401. Each request records its token and checks the
    /// keychain already holds it: a rotated pair is written before use.
    struct Api {
        token: String,
        ok: &'static str,
        keychain: Arc<Keychain>,
        sent_with: Arc<Mutex<Vec<String>>>,
    }

    impl Transport for Api {
        fn send(&self, _: &Value) -> Result<Response, String> {
            self.sent_with.lock().unwrap().push(self.token.clone());
            if self.token != self.ok {
                return Ok(Response {
                    status: 401,
                    body: r#"{"message":"Bad credentials"}"#.to_owned(),
                    ..Default::default()
                });
            }
            assert_eq!(
                stored_access(&self.keychain),
                self.token,
                "stored before use"
            );
            Ok(Response {
                status: 200,
                body: format!(
                    r#"{{"data":{{"repository":{{"defaultBranchRef":{{"name":"main","target":{{"oid":"{HEAD}"}}}}}}}}}}"#
                ),
                ..Default::default()
            })
        }
    }

    /// A forge on the app's token stored in `keychain`, GitHub accepting
    /// `ok` alone.
    fn app_forge(
        keychain: Arc<Keychain>,
        script: &'static Script,
        lock: PathBuf,
        ok: &'static str,
    ) -> (GitHubForge, Arc<Mutex<Vec<String>>>) {
        let stored = keychain.read(ACCOUNT).unwrap().unwrap();
        let source = AppToken::new(
            keychain.clone(),
            ACCOUNT,
            lock,
            DeviceFlow::new(script, "Iv1.test").with_clock(now),
            &stored,
        )
        .unwrap()
        .with_clock(now)
        .with_wake(|| Wake::Full);
        let sent_with = Arc::new(Mutex::new(Vec::new()));
        let seen = sent_with.clone();
        let connect: Connect = Box::new(move |token: Token| {
            // The token is matched by trying each one GitHub accepts.
            let token = ["ghu_old", "ghu_new", "ghu_peer"]
                .into_iter()
                .find(|known| Token::new(*known) == token)
                .expect("a scripted token")
                .to_owned();
            Box::new(Api {
                token,
                ok,
                keychain: keychain.clone(),
                sent_with: seen.clone(),
            })
        });
        let repo = Repo::parse("owner/repo").unwrap();
        (GitHubForge::with_app(source, connect, repo), sent_with)
    }

    fn leaked(script: Script) -> &'static Script {
        Box::leak(Box::new(script))
    }

    /// Criterion 1: a token past its stored expiry is refreshed once, the
    /// new pair stored before it is sent, and the next reads keep it.
    #[test]
    fn an_expired_token_is_refreshed_once_and_stored_before_use() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Arc::new(Keychain::in_memory());
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let script = leaked(Script::answering(&[&granted("ghu_new", "ghr_new")]));
        let (forge, sent_with) =
            app_forge(keychain.clone(), script, dir.path().join("lock"), "ghu_new");
        assert!(forge.default_branch().is_ok());
        assert!(forge.default_branch().is_ok());
        assert_eq!(script.sent(), ["ghr_old"]);
        assert_eq!(*sent_with.lock().unwrap(), ["ghu_new", "ghu_new"]);
        assert_eq!(stored_access(&keychain), "ghu_new");
    }

    /// Criterion 2: a 401 while a refresh token is stored gets one refresh
    /// and the read once more; with no refresh token, the 401 stands and
    /// nothing is sent to the token endpoint.
    #[test]
    fn a_401_is_refreshed_once_while_a_refresh_token_is_stored() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Arc::new(Keychain::in_memory());
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 3600))
            .unwrap();
        let script = leaked(Script::answering(&[&granted("ghu_new", "ghr_new")]));
        let (forge, sent_with) =
            app_forge(keychain.clone(), script, dir.path().join("lock"), "ghu_new");
        assert!(forge.default_branch().is_ok());
        assert_eq!(script.sent(), ["ghr_old"]);
        assert_eq!(*sent_with.lock().unwrap(), ["ghu_old", "ghu_new"]);

        let keychain = Arc::new(Keychain::in_memory());
        keychain
            .store(ACCOUNT, &entry("ghu_old", None, 3600))
            .unwrap();
        let script = leaked(Script::default());
        let (forge, _) = app_forge(keychain, script, dir.path().join("lock"), "ghu_new");
        assert_eq!(
            forge.default_branch().unwrap_err().kind,
            ErrorKind::Unauthorized
        );
        assert!(script.sent().is_empty());
    }

    /// A 401 because another process rotated the pair: the stored pair,
    /// read again under the lock, is used, and no refresh is sent.
    #[test]
    fn a_pair_another_process_stored_is_used_without_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Arc::new(Keychain::in_memory());
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 3600))
            .unwrap();
        let script = leaked(Script::default());
        let (forge, sent_with) = app_forge(
            keychain.clone(),
            script,
            dir.path().join("lock"),
            "ghu_peer",
        );
        keychain
            .store(ACCOUNT, &entry("ghu_peer", Some("ghr_peer"), 28_800))
            .unwrap();
        assert!(forge.default_branch().is_ok());
        assert!(script.sent().is_empty());
        assert_eq!(*sent_with.lock().unwrap(), ["ghu_old", "ghu_peer"]);
    }

    /// Criterion 3: two processes find the token expired at once. The first
    /// refreshes under the account lock while the second waits for it; the
    /// second then reads the stored pair and sends no refresh.
    #[test]
    fn two_processes_refreshing_at_once_send_one_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("project").join("github-app.lock");
        let keychain = Keychain::in_memory();
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let (started_tx, started) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let script = Script {
            hold: Some((Mutex::new(started_tx), Mutex::new(release_rx))),
            ..Script::answering(&[&granted("ghu_new", "ghr_new")])
        };
        let mut first = source(&keychain, &script, lock.clone());
        let mut second = source(&keychain, &script, lock);
        thread::scope(|scope| {
            let first = scope.spawn(move || first.token_at(now()));
            started.recv().unwrap();
            let (done_tx, done) = mpsc::channel();
            let second = scope.spawn(move || {
                let token = second.token_at(now());
                done_tx.send(()).unwrap();
                token
            });
            assert!(
                done.recv_timeout(Duration::from_millis(200)).is_err(),
                "the second waits for the account lock"
            );
            release.send(()).unwrap();
            let new = Token::new("ghu_new");
            assert_eq!(first.join().unwrap().unwrap(), new);
            assert_eq!(second.join().unwrap().unwrap(), new);
        });
        assert_eq!(script.sent(), ["ghr_old"]);
        assert_eq!(stored_access(&keychain), "ghu_new");
    }

    /// Criterion 4: a refused refresh stops with `owlshift init`, and its
    /// refresh token is not sent again; so does a refresh token past its
    /// life, unsent. No token is in the words.
    #[test]
    fn a_refused_refresh_says_sign_in_again_once() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let script = Script::answering(&[REFUSED]);
        let mut token = source(&keychain, &script, dir.path().join("lock"));
        for _ in 0..2 {
            let error = token.token_at(now()).unwrap_err();
            assert_eq!(error.kind, ErrorKind::SignInAgain);
            assert!(error.message.contains("`owlshift init`"), "{error}");
            assert!(error.message.contains(ACCOUNT), "{error}");
            assert!(!error.message.contains("ghr_") && !error.message.contains("ghu_"));
        }
        assert_eq!(script.sent(), ["ghr_old"]);
        let shown = format!("{token:?}");
        assert!(
            !shown.contains("ghu_") && !shown.contains("ghr_"),
            "{shown}"
        );

        let script = Script::default();
        let mut token = source(&keychain, &script, dir.path().join("lock"));
        let past = at(15_724_800);
        let error = token.token_at(past).unwrap_err();
        assert_eq!(error.kind, ErrorKind::SignInAgain);
        assert!(error.message.contains("past its life"), "{error}");
        assert!(script.sent().is_empty());
    }

    /// Criterion 4: GitHub rotated the pair but the keychain write failed
    /// every try: the rotation is lost, the new token never handed out, and
    /// the spent refresh token not sent again. OWL-649: each try is an
    /// event, the last saying the rotation is lost.
    #[test]
    fn a_lost_rotation_says_sign_in_again() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        let stored = entry("ghu_old", Some("ghr_old"), 0);
        keychain.store(ACCOUNT, &stored).unwrap();
        let script = Script::answering(&[&granted("ghu_new", "ghr_new")]);
        let entries = Faulty::new(&keychain, Fault::StoreFails);
        let flow = DeviceFlow::new(&script, "Iv1.test").with_clock(now);
        let mut token = AppToken::new(&entries, ACCOUNT, dir.path().join("lock"), flow, &stored)
            .unwrap()
            .with_events(recorder(dir.path()))
            .with_wake(|| Wake::Full);
        let error = token.token_at(now()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::SignInAgain);
        assert!(error.message.contains("the rotation is lost"), "{error}");
        assert!(error.message.contains("`owlshift init`"), "{error}");
        assert_eq!(entries.stores(), STORE_TRIES);
        assert_eq!(
            token.token_at(now()).unwrap_err().kind,
            ErrorKind::SignInAgain
        );
        assert_eq!(script.sent(), ["ghr_old"]);
        assert_eq!(stored_access(&keychain), "ghu_old");

        let events = logged(dir.path());
        assert_eq!(
            told(&events),
            [
                "refresh sending",
                "refresh rotated",
                "keychain_write failed",
                "keychain_write failed",
                "keychain_write failed",
                "refresh not_sent refused_before",
            ]
        );
        let writes: Vec<&Event> = events
            .iter()
            .filter(|e| e.data["github_app"] == "keychain_write")
            .collect();
        for (n, write) in writes.iter().enumerate() {
            assert_eq!(write.kind, EventKind::Warning);
            assert_eq!(write.data["by"], "refresh");
            assert_eq!(write.data["try"], n + 1);
            assert_eq!(write.data["tries"], STORE_TRIES);
            assert_eq!(write.data["why"], "the keychain is locked");
            assert_eq!(write.data["expires_at"], at(28_800).to_string());
            assert_eq!(write.data.get("rotation_lost").is_some(), n == 2, "{n}");
        }
    }

    /// The events of a refresh in the log of `data`, for the project
    /// `owner/repo`.
    fn recorder(data: &std::path::Path) -> TokenEvents {
        TokenEvents::new(EventLog::in_dir(data), "owner/repo")
    }

    /// OWL-649, criterion 1: a refresh records `sending`, then `rotated`,
    /// then the write, each a decision of this process holding the stored
    /// pair's expiry; `owlshift logs` prints them.
    #[test]
    fn every_refresh_records_its_outcome_and_the_stored_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let script = Script::answering(&[&granted("ghu_new", "ghr_new")]);
        let mut token =
            source(&keychain, &script, dir.path().join("lock")).with_events(recorder(dir.path()));
        assert_eq!(token.token_at(now()).unwrap(), Token::new("ghu_new"));

        let events = logged(dir.path());
        assert_eq!(
            told(&events),
            [
                "refresh sending",
                "refresh rotated",
                "keychain_write stored"
            ]
        );
        for event in &events {
            assert_eq!(event.kind, EventKind::Decision, "{event:?}");
            assert_eq!(event.project, "owner/repo");
            assert_eq!((event.ticket.as_ref(), event.run.as_ref()), (None, None));
            assert_eq!(event.data["pid"], std::process::id());
            assert_eq!(event.data["version"], OWLSHIFT_VERSION);
            assert!(event.data["command"].is_string(), "{event:?}");
        }
        let stored = at(0).to_string();
        for refresh in &events[..2] {
            assert_eq!(refresh.data["trigger"], "expired");
            assert_eq!(refresh.data["stored_expires_at"], stored);
            assert_eq!(refresh.data["held_expires_at"], stored);
        }
        assert_eq!(events[1].data["expires_at"], at(28_800).to_string());
        assert_eq!(
            events[1].data["refresh_expires_at"],
            at(15_724_800).to_string()
        );
        assert_eq!(events[2].data["by"], "refresh");
        assert_eq!(events[2].data["expires_at"], at(28_800).to_string());

        let mut printed = Vec::new();
        let log = EventLog::in_dir(dir.path());
        events::print(log.path(), None, &mut printed, &mut Vec::new(), None, None).unwrap();
        let printed = String::from_utf8(printed).unwrap();
        assert!(
            printed.contains("github_app=refresh") && printed.contains("outcome=rotated"),
            "{printed}"
        );
    }

    /// OWL-649: a pair a peer stored, a refusal, no answer and a refresh
    /// token past its life each leave their events; a refusal is told once
    /// more, as `refused_before`, however many calls follow it.
    #[test]
    fn each_refresh_says_how_it_ended_or_why_none_was_sent() {
        let run = |stored: Secret, answers: &[&str], calls: usize, at: Timestamp| {
            let dir = tempfile::tempdir().unwrap();
            let keychain = Keychain::in_memory();
            keychain.store(ACCOUNT, &stored).unwrap();
            let script = Script::answering(answers);
            let mut token = source(&keychain, &script, dir.path().join("lock"))
                .with_events(recorder(dir.path()));
            for _ in 0..calls {
                let _ = token.token_at(at);
            }
            logged(dir.path())
        };
        let expired = || entry("ghu_old", Some("ghr_old"), 0);

        let events = run(expired(), &[REFUSED], 3, now());
        assert_eq!(
            told(&events),
            [
                "refresh sending",
                "refresh refused",
                "refresh not_sent refused_before"
            ]
        );
        assert_eq!(events[1].kind, EventKind::Warning);
        assert_eq!(events[1].data["what"], "github_app_token");
        assert!(
            events[1].data["why"]
                .as_str()
                .unwrap()
                .contains("GitHub refused the GitHub App's refresh token (bad_refresh_token)"),
            "{:?}",
            events[1]
        );

        let events = run(expired(), &[NO_ANSWER], 1, now());
        assert_eq!(told(&events), ["refresh sending", "refresh no_answer"]);
        assert_eq!(events[1].kind, EventKind::Warning);

        let events = run(expired(), &[], 1, at(15_724_800));
        assert_eq!(told(&events), ["refresh not_sent past_its_life"]);

        // A peer's pair, stored while this source held the old one.
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        keychain.store(ACCOUNT, &expired()).unwrap();
        let script = Script::default();
        let mut token =
            source(&keychain, &script, dir.path().join("lock")).with_events(recorder(dir.path()));
        keychain
            .store(ACCOUNT, &entry("ghu_peer", Some("ghr_peer"), 28_800))
            .unwrap();
        assert_eq!(token.token_at(now()).unwrap(), Token::new("ghu_peer"));
        let events = logged(dir.path());
        assert_eq!(told(&events), ["refresh stored_pair_used"]);
        assert_eq!(events[0].kind, EventKind::Decision);
        assert_eq!(events[0].data["stored_expires_at"], at(28_800).to_string());
        assert!(script.sent().is_empty());
    }

    /// OWL-649 (F11, F14 of its plan's review): each call that stops before a
    /// refresh, for want of the lock or of a stored pair it can read,
    /// records its reason once, with the expiry of the pair the process
    /// holds and, having read none, no stored expiry.
    #[test]
    fn a_refresh_stopped_before_the_request_records_why() {
        /// The reason, what changes the keychain after the source read it,
        /// how its entries fail, and whether a stored pair is read.
        type Case<'a> = (&'a str, &'a dyn Fn(&Keychain), Option<Fault>, bool);
        let held = entry("ghu_old", Some("ghr_old"), 0);
        let cases: [Case; 5] = [
            (
                "not_stored",
                &|k| k.delete(ACCOUNT).map(|_| ()).unwrap(),
                None,
                false,
            ),
            (
                "unreadable",
                &|k| k.store(ACCOUNT, &Secret::new("garbage")).unwrap(),
                None,
                false,
            ),
            (
                "keychain_unreadable",
                &|_| {},
                Some(Fault::ReadFails),
                false,
            ),
            (
                "no_refresh_token",
                &|k| k.store(ACCOUNT, &entry("ghu_old", None, 0)).unwrap(),
                None,
                true,
            ),
            ("lock_failed", &|_| {}, None, false),
        ];
        for (reason, change, fault, read) in cases {
            let dir = tempfile::tempdir().unwrap();
            let keychain = Keychain::in_memory();
            keychain.store(ACCOUNT, &held).unwrap();
            change(&keychain);
            let entries = Faulty::new(&keychain, fault.unwrap_or(Fault::StoreFails));
            let lock = if reason == "lock_failed" {
                // A lock whose folder is a file cannot be taken.
                fs::write(dir.path().join("file"), "").unwrap();
                dir.path().join("file").join("lock")
            } else {
                dir.path().join("lock")
            };
            let script = Script::default();
            let flow = DeviceFlow::new(&script, "Iv1.test").with_clock(now);
            let mut token = AppToken::new(&entries, ACCOUNT, lock, flow, &held)
                .unwrap()
                .with_events(recorder(dir.path()))
                .with_wake(|| Wake::Full);
            for _ in 0..2 {
                assert!(token.token_at(now()).is_err(), "{reason}");
            }
            assert!(script.sent().is_empty(), "{reason}");
            let events = logged(dir.path());
            assert_eq!(
                told(&events),
                [format!("refresh not_sent {reason}")],
                "{reason}"
            );
            let event = &events[0];
            assert_eq!(event.kind, EventKind::Warning, "{reason}");
            assert!(event.data["why"].is_string(), "{reason}");
            assert_eq!(event.data["held_expires_at"], at(0).to_string(), "{reason}");
            assert_eq!(
                event.data.get("stored_expires_at").is_some(),
                read,
                "{reason}"
            );
        }
    }

    /// OWL-649's reproduction of the incident: GitHub rotated the pair, and
    /// its answer was lost. The source keeps the spent refresh token, sends
    /// it at the next call, and GitHub refuses it as the incident's log
    /// says; the two `sending` events hold the same stored expiry.
    #[test]
    fn an_answer_lost_after_github_rotated_is_refused_at_the_next_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let github = Rotating::new("ghr_old", &[Answer::Lost]);
        let stored = keychain.read(ACCOUNT).unwrap().unwrap();
        let flow = DeviceFlow::new(&github, "Iv1.test").with_clock(now);
        let mut token = AppToken::new(&keychain, ACCOUNT, dir.path().join("lock"), flow, &stored)
            .unwrap()
            .with_events(recorder(dir.path()))
            .with_wake(|| Wake::Full);

        let error = token.token_at(now()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unreachable, "{error}");
        assert_eq!(stored_access(&keychain), "ghu_old");
        let error = token.token_at(now()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::SignInAgain, "{error}");
        assert!(
            error.message.contains(
                "GitHub refused the GitHub App's refresh token (incorrect_client_credentials)"
            ),
            "{error}"
        );
        assert_eq!(github.sent(), ["ghr_old", "ghr_old"]);

        let events = logged(dir.path());
        assert_eq!(
            told(&events),
            [
                "refresh sending",
                "refresh no_answer",
                "refresh sending",
                "refresh refused"
            ]
        );
        assert_eq!(
            events[0].data["stored_expires_at"],
            events[2].data["stored_expires_at"]
        );
    }

    /// A source on the GitHub `github`, recording in the log of `data`, in
    /// a full wake.
    fn rotating_source<'a>(
        keychain: &'a Keychain,
        github: &'a Rotating,
        data: &std::path::Path,
    ) -> AppToken<&'a Keychain, DeviceFlow<&'a Rotating>> {
        let stored = keychain.read(ACCOUNT).unwrap().unwrap();
        let flow = DeviceFlow::new(github, "Iv1.test").with_clock(now);
        AppToken::new(keychain, ACCOUNT, data.join("lock"), flow, &stored)
            .unwrap()
            .with_events(recorder(data))
            .with_wake(|| Wake::Full)
    }

    /// OWL-649, criterion 3: in a dark wake no refresh is sent, each call
    /// failing as one GitHub did not answer, in words that name the dark
    /// wake, and the hold told once; the first call in a full wake sends it.
    #[test]
    fn a_dark_wake_sends_no_refresh_until_the_full_wake() {
        static DARK: AtomicBool = AtomicBool::new(true);
        fn probe() -> Wake {
            if DARK.load(Ordering::SeqCst) {
                Wake::Dark
            } else {
                Wake::Full
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let github = Rotating::new("ghr_old", &[]);
        let mut token = rotating_source(&keychain, &github, dir.path()).with_wake(probe);
        for _ in 0..2 {
            let error = token.token_at(now()).unwrap_err();
            assert_eq!(error.kind, ErrorKind::Unreachable, "{error}");
            assert!(
                error
                    .message
                    .contains("not refreshed: the machine is in a dark wake"),
                "{error}"
            );
        }
        assert!(github.sent().is_empty());
        assert_eq!(stored_access(&keychain), "ghu_old");
        let events = logged(dir.path());
        assert_eq!(told(&events), ["refresh not_sent dark_wake"]);
        assert_eq!(events[0].kind, EventKind::Decision);
        assert_eq!(events[0].data["stored_expires_at"], at(0).to_string());

        DARK.store(false, Ordering::SeqCst);
        assert_eq!(token.token_at(now()).unwrap(), Token::new("ghu_1"));
        assert_eq!(github.sent(), ["ghr_old"]);
        assert_eq!(stored_access(&keychain), "ghu_1");
        let events = logged(dir.path());
        assert_eq!(
            told(&events),
            [
                "refresh not_sent dark_wake",
                "refresh sending",
                "refresh rotated",
                "keychain_write stored"
            ]
        );
        assert_eq!(events[1].data["wake"], "full");
    }

    /// In a dark wake a pair a peer stored is still used, nothing sent;
    /// using it ends the stretch, so the next hold is told again.
    #[test]
    fn a_dark_wake_still_uses_a_peers_pair_and_tells_each_hold() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let github = Rotating::new("ghr_old", &[]);
        let mut token = rotating_source(&keychain, &github, dir.path()).with_wake(|| Wake::Dark);
        assert!(token.token_at(now()).is_err());
        keychain
            .store(ACCOUNT, &entry("ghu_peer", Some("ghr_peer"), 28_800))
            .unwrap();
        assert_eq!(token.token_at(now()).unwrap(), Token::new("ghu_peer"));
        // The peer's pair past its expiry, the machine still dark.
        let error = token.token_at(at(28_800)).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unreachable, "{error}");
        assert!(github.sent().is_empty());
        assert_eq!(
            told(&logged(dir.path())),
            [
                "refresh not_sent dark_wake",
                "refresh stored_pair_used",
                "refresh not_sent dark_wake"
            ]
        );
    }

    /// The troubleshooting page quotes the hold's words as the code writes
    /// them.
    #[test]
    fn the_troubleshooting_page_quotes_the_dark_wake_hold() {
        let page =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/troubleshooting.md");
        let page = fs::read_to_string(page).unwrap();
        assert!(page.contains(DARK_WAKE_HOLD), "{DARK_WAKE_HOLD}");
    }

    /// A reading the probe cannot make holds nothing, as OWL-485's does.
    #[test]
    fn an_unknown_wake_holds_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let github = Rotating::new("ghr_old", &[]);
        let mut token = rotating_source(&keychain, &github, dir.path()).with_wake(|| Wake::Unknown);
        assert_eq!(token.token_at(now()).unwrap(), Token::new("ghu_1"));
        assert_eq!(github.sent(), ["ghr_old"]);
        let events = logged(dir.path());
        assert_eq!(events[0].data["outcome"], "sending");
        assert_eq!(events[0].data["wake"], "unknown");
    }

    #[test]
    fn the_command_is_a_plain_word_or_owlshift() {
        let of = |arg: Option<&str>| command_name(arg.map(OsString::from));
        assert_eq!(of(Some("watch")), "watch");
        assert_eq!(of(Some("continue")), "continue");
        assert_eq!(of(Some("doctor-x")), "doctor-x");
        for odd in [
            None,
            Some(""),
            Some("--envelope"),
            Some("Watch"),
            Some("github_token"),
            Some("ghu_secret"),
            Some("a-very-long-word-past-the-limit"),
        ] {
            assert_eq!(of(odd), "owlshift", "{odd:?}");
        }
    }

    /// A refresh with no usable answer may be tried again: it is no
    /// refusal. A lock held past the wait is reported, nothing sent. Run
    /// alone (`alone::run_alone`, OWL-414): the test takes at once the lock
    /// the refreshes released, which another test's child spawned meanwhile
    /// would make look held.
    #[test]
    fn a_failed_refresh_and_a_held_lock_are_tried_again_later() {
        crate::alone::run_alone(
            "github_token::tests::helper_a_failed_refresh_and_a_held_lock_are_tried_again_later",
        );
    }

    #[test]
    #[ignore = "helper, run by the test above"]
    fn helper_a_failed_refresh_and_a_held_lock_are_tried_again_later() {
        if !crate::alone::running_alone() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        let keychain = Keychain::in_memory();
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let script = Script::answering(&[
            r#"{"error":"temporarily_unavailable"}"#,
            &granted("ghu_new", "ghr_new"),
        ]);
        let mut token = source(&keychain, &script, lock.clone());
        assert_eq!(token.token_at(now()).unwrap_err().kind, ErrorKind::Other);
        assert_eq!(token.token_at(now()).unwrap(), Token::new("ghu_new"));
        assert_eq!(script.sent(), ["ghr_old", "ghr_old"]);

        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let script = Script::default();
        let mut token = source(&keychain, &script, lock.clone()).with_events(recorder(dir.path()));
        token.lock_wait = Duration::from_millis(50);
        let _held = lock_file_within(&lock, Duration::ZERO, ACCOUNT_LOCK_POLL)
            .unwrap()
            .unwrap();
        let error = token.token_at(now()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Other);
        assert!(error.message.contains("account lock"), "{error}");
        assert!(script.sent().is_empty());
        assert_eq!(told(&logged(dir.path())), ["refresh not_sent lock_held"]);
    }

    /// A refresh GitHub gave no answer to is its own kind (OWL-479), and
    /// the token is tried again next time.
    #[test]
    fn a_refresh_with_no_answer_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let script = Script::answering(&[NO_ANSWER, &granted("ghu_new", "ghr_new")]);
        let mut token = source(&keychain, &script, dir.path().join("lock"));
        let error = token.token_at(now()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unreachable);
        assert!(error.message.contains("could not be reached"), "{error}");
        assert_eq!(token.token_at(now()).unwrap(), Token::new("ghu_new"));
    }

    /// An entry that does not read, or is gone when the refresh reads it
    /// again, says sign in again.
    #[test]
    fn an_unreadable_or_removed_entry_says_sign_in_again() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        let script = Script::default();
        let flow = DeviceFlow::new(&script, "Iv1.test");
        let error = AppToken::new(
            &keychain,
            ACCOUNT,
            dir.path().join("lock"),
            flow,
            &Secret::new("ghu_SENTINEL"),
        )
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::SignInAgain);
        assert!(!error.message.contains("SENTINEL"), "{error}");

        keychain
            .store(ACCOUNT, &entry("ghu_old", Some("ghr_old"), 0))
            .unwrap();
        let mut token = source(&keychain, &script, dir.path().join("lock"));
        keychain.delete(ACCOUNT).unwrap();
        let error = token.token_at(now()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::SignInAgain);
        assert!(error.message.contains("no longer stored"), "{error}");
    }
}
