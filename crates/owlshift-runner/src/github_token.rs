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
//!
//! No token reaches an error or `Debug` output.

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
use owlshift_platform::keychain::{Keychain, Secret};

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
}

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
        })
    }

    /// The same source on the clock `now`, which [`TokenSource`] reads.
    pub fn with_clock(self, now: fn() -> Timestamp) -> Self {
        Self { now, ..self }
    }

    /// The token to send at `now`: the one held, refreshed first when it is
    /// past its stored expiry.
    pub fn token_at(&mut self, now: Timestamp) -> Result<Token, Error> {
        if !self.held.expired(now) {
            return Ok(Token::new(self.held.access()));
        }
        let stale = Token::new(self.held.access());
        self.renew(&stale, now)?
            .ok_or_else(|| self.sign_in_again("has expired and holds no refresh token"))
    }

    /// GitHub answered HTTP 401 to `refused` at `now`: the token to send
    /// once more, or `None` when no refresh token is stored.
    pub fn refused_at(&mut self, refused: &Token, now: Timestamp) -> Result<Option<Token>, Error> {
        self.renew(refused, now)
    }

    /// Under the account lock: the stored pair when another process stored
    /// one in place of `stale`, else a refresh of the stored pair, written
    /// before it is used; `None` when the stored pair has no refresh token.
    fn renew(&mut self, stale: &Token, now: Timestamp) -> Result<Option<Token>, Error> {
        let _lock = self.account_lock()?;
        let stored = match self.entries.read(&self.account) {
            Ok(Some(entry)) => UserToken::from_entry(entry.expose())
                .map_err(|_| self.sign_in_again("does not read"))?,
            Ok(None) => return Err(self.sign_in_again("is no longer stored")),
            Err(error) => {
                return Err(Error::new(
                    ErrorKind::Other,
                    format!(
                        "the GitHub App's token could not be read from the system keychain: \
                         {error}"
                    ),
                ));
            }
        };
        if Token::new(stored.access()) != *stale && !stored.expired(now) {
            self.held = stored;
            return Ok(Some(Token::new(self.held.access())));
        }
        let Some(refresh) = stored.refresh().map(str::to_owned) else {
            self.held = stored;
            return Ok(None);
        };
        if self.dead.as_deref() == Some(refresh.as_str()) {
            return Err(self.sign_in_again("was refused its refresh"));
        }
        if stored.refresh_expires_at().is_some_and(|at| now >= at) {
            return Err(self.sign_in_again("has a refresh token past its life"));
        }
        // Spent from here on: GitHub may rotate even if its answer is lost.
        self.dead = Some(refresh.clone());
        let new = match self.flow.refresh(&refresh) {
            Ok(new) => new,
            Err(refused @ AppError::Refused(_)) => {
                return Err(self.sign_in_again(&format!("could not be refreshed: {refused}")));
            }
            Err(other) => {
                // No answer, or one a retry may clear: the token is tried
                // again next time; a rotation GitHub made meanwhile shows as
                // a refused refresh then. No answer at all is its own kind,
                // which a watch starting waits on (OWL-479).
                self.dead = None;
                let kind = match other {
                    AppError::Unreachable(_) => ErrorKind::Unreachable,
                    _ => ErrorKind::Other,
                };
                return Err(Error::new(
                    kind,
                    format!("the GitHub App's token could not be refreshed: {other}"),
                ));
            }
        };
        self.write(&new)?;
        self.held = new;
        Ok(Some(Token::new(self.held.access())))
    }

    /// Writes `new` under the account, tried [`STORE_TRIES`] times.
    fn write(&self, new: &UserToken) -> Result<(), Error> {
        let entry = Secret::new(new.to_entry());
        let mut tries = 0;
        loop {
            tries += 1;
            match self.entries.store(&self.account, &entry) {
                Ok(()) => return Ok(()),
                Err(error) if tries >= STORE_TRIES => {
                    return Err(self.sign_in_again(&format!(
                        "was rotated by GitHub, but storing the new one failed ({error}), so the \
                         rotation is lost"
                    )));
                }
                Err(_) => thread::sleep(STORE_PAUSE),
            }
        }
    }

    /// The account lock, waited for at most `lock_wait`.
    fn account_lock(&self) -> Result<fs::File, Error> {
        let failed = |error: std::io::Error| {
            Error::new(
                ErrorKind::Other,
                format!(
                    "the GitHub App's account lock {} could not be taken: {error}",
                    self.lock.display()
                ),
            )
        };
        if let Some(parent) = self.lock.parent() {
            fs::create_dir_all(parent).map_err(failed)?;
        }
        lock_file_within(&self.lock, self.lock_wait, ACCOUNT_LOCK_POLL)
            .map_err(failed)?
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Other,
                    format!(
                        "another Owlshift process held the GitHub App's account lock {} for \
                         more than {} s, refreshing its token: try again",
                        self.lock.display(),
                        self.lock_wait.as_secs()
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

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::mpsc;

    use owlshift_adapters::forge::Repo;
    use owlshift_adapters::forge::github::{Connect, GitHubForge, Response, Transport};
    use owlshift_adapters::forge::github_app::LoginAnswer;
    use serde_json::Value;

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
        .with_clock(now);
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

    /// Entries whose writes always fail.
    struct Unwritable<'a>(&'a Keychain, Mutex<u32>);

    impl Entries for Unwritable<'_> {
        fn read(&self, account: &str) -> Result<Option<Secret>, String> {
            Entries::read(self.0, account)
        }

        fn store(&self, _: &str, _: &Secret) -> Result<(), String> {
            *self.1.lock().unwrap() += 1;
            Err("the keychain is locked".to_owned())
        }
    }

    /// Criterion 4: GitHub rotated the pair but the keychain write failed
    /// every try: the rotation is lost, the new token never handed out, and
    /// the spent refresh token not sent again.
    #[test]
    fn a_lost_rotation_says_sign_in_again() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain::in_memory();
        let stored = entry("ghu_old", Some("ghr_old"), 0);
        keychain.store(ACCOUNT, &stored).unwrap();
        let script = Script::answering(&[&granted("ghu_new", "ghr_new")]);
        let entries = Unwritable(&keychain, Mutex::new(0));
        let flow = DeviceFlow::new(&script, "Iv1.test").with_clock(now);
        let mut token =
            AppToken::new(&entries, ACCOUNT, dir.path().join("lock"), flow, &stored).unwrap();
        let error = token.token_at(now()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::SignInAgain);
        assert!(error.message.contains("the rotation is lost"), "{error}");
        assert!(error.message.contains("`owlshift init`"), "{error}");
        assert_eq!(*entries.1.lock().unwrap(), STORE_TRIES);
        assert_eq!(
            token.token_at(now()).unwrap_err().kind,
            ErrorKind::SignInAgain
        );
        assert_eq!(script.sent(), ["ghr_old"]);
        assert_eq!(stored_access(&keychain), "ghu_old");
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
        let mut token = source(&keychain, &script, lock.clone());
        token.lock_wait = Duration::from_millis(50);
        let _held = lock_file_within(&lock, Duration::ZERO, ACCOUNT_LOCK_POLL)
            .unwrap()
            .unwrap();
        let error = token.token_at(now()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Other);
        assert!(error.message.contains("account lock"), "{error}");
        assert!(script.sent().is_empty());
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
